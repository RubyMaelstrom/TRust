//! Production Lumen JavaScript backend and its synthetic benchmark harness.
//!
//! The resident actor shares TRust's platform prelude, DOM arena, and integer-handle host boundary
//! with the legacy Boa implementation. Both engine adapters are checked against the same
//! engine-neutral registry so host names and JavaScript-visible function lengths cannot drift.

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

struct LumenGeomCache {
    epoch: u64,
    boxes: std::collections::HashMap<crate::dom::NodeId, crate::layout2::PxRect>,
    tracks: std::collections::HashMap<crate::dom::NodeId, (Vec<f32>, Vec<f32>)>,
    scrolling_areas: std::collections::HashMap<crate::dom::NodeId, crate::layout2::PxRect>,
    paint: Option<crate::render::PagePaint>,
    /// The cached boxes belonging to the container Document remain valid. A nested Document may
    /// have advanced the arena-wide epoch without affecting these boxes (HTML §7.3.1.3).
    top_document_valid: bool,
}

impl LumenGeomCache {
    fn empty() -> Self {
        Self {
            epoch: u64::MAX,
            boxes: Default::default(),
            tracks: Default::default(),
            scrolling_areas: Default::default(),
            paint: None,
            top_document_valid: false,
        }
    }
}

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
#[allow(dead_code)] // Some task variants are exercised only by particular web-platform features.
enum LumenHostTask {
    FetchDone {
        id: usize,
        result: LumenFetchResult,
    },
    ResourceDone {
        context: u64,
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
    pending_fetches: HashMap<usize, LumenPendingFetch>,
}

struct LumenPendingFetch {
    /// Fetch's task destination: the environment whose global receives the
    /// networking task and owns the resulting Response/Body objects.
    context: u64,
    resolve: Value,
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
    hit_testing_active: Cell<bool>,
    images: Rc<RefCell<crate::layout2::ImageSizes>>,
    task_events: Option<tokio::sync::mpsc::UnboundedSender<LumenHostTask>>,
    pending_resources: usize,
    pending_dynamic_modules: Arc<std::sync::atomic::AtomicUsize>,
    network: Option<LumenNetwork>,
    websockets: Option<LumenWebSockets>,
    workers: Option<LumenPageWorkers>,
    worker_self: Option<LumenWorkerSelf>,
    wasm: lumen_wasm::PageWasm,
    next_window_context: u64,
    window_realms: HashMap<u64, Value>,
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
            geom_cache: Rc::new(RefCell::new(LumenGeomCache::empty())),
            hit_testing_active: Cell::new(false),
            images: Default::default(),
            task_events: None,
            pending_resources: 0,
            pending_dynamic_modules: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            network: None,
            websockets: None,
            workers: None,
            worker_self: None,
            wasm: lumen_wasm::PageWasm::new(),
            next_window_context: 1,
            window_realms: HashMap::new(),
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

fn engine_call_trust_method(
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

/// Dispatch one HTML timer task with the author callback as the engine entry point.
///
/// ECMA-262 §9.5 requires a host job to begin when the Agent's execution-context stack is empty;
/// HTML §8.7 then invokes the stored handler with the WindowProxy callback-this value and its
/// trailing arguments. The platform prelude selects the task and owns its nesting/repeat/frame
/// bookkeeping, but calling the handler here avoids placing those self-hosted helper frames below
/// author code. `null` from the selector means an animation-frame opportunity won the ordering
/// race, which remains a batched JavaScript algorithm and falls back to `tickTo`.
fn dispatch_timer_task_to(engine: &mut lumen::Engine, deadline: f64) -> Result<bool, EvalError> {
    let selected = engine_call_trust_method(engine, "takeTimerTaskTo", &[Value::Num(deadline)])?;
    match selected {
        Value::Bool(false) => return Ok(false),
        Value::Null => {
            let ran = engine_call_trust_method(engine, "tickTo", &[Value::Num(deadline)])?;
            return Ok(ran.as_num_opt().is_some_and(|count| count > 0.0));
        }
        Value::Obj(_) => {}
        other => {
            let message = format!(
                "timer selector returned {}, expected task, null, or false",
                other.type_of()
            );
            let error = engine.ctx().make_error("TypeError", message);
            return Err(EvalError::Throw(error));
        }
    }

    let args_value = engine
        .ctx()
        .member_get(&selected, "args")
        .map_err(EvalError::Throw)?;
    let length = engine
        .ctx()
        .member_get(&args_value, "length")
        .map_err(EvalError::Throw)?
        .as_num_opt()
        .unwrap_or(0.0);
    // HTML invokes the handler with the task's stored variadic argument list;
    // the standard does not impose an embedder-selected element count. The
    // platform array has an integral ECMAScript length, so only reject values
    // that cannot be represented as a host allocation index.
    if !length.is_finite() || length < 0.0 || length.fract() != 0.0 || length > usize::MAX as f64 {
        let error = engine.ctx().make_error(
            "RangeError",
            "timer callback argument list has an invalid length",
        );
        return Err(EvalError::Throw(error));
    }
    let mut args = Vec::with_capacity(length as usize);
    for index in 0..length as usize {
        args.push(
            engine
                .ctx()
                .member_get(&args_value, &index.to_string())
                .map_err(EvalError::Throw)?,
        );
    }

    let handler =
        engine_call_trust_method(engine, "beginTimerTask", std::slice::from_ref(&selected))?;
    let callback_this = engine
        .ctx()
        .member_get(&selected, "__trustTimerThis")
        .unwrap_or_else(|_| engine.global_this());
    let (callback_error, callback_failed, callback_interrupted) =
        match engine.call_function_interruptible(&handler, callback_this, &args) {
            Ok(_) => (Value::Undefined, false, None),
            Err(EvalError::Throw(error)) => (error, true, None),
            Err(EvalError::Interrupted(reason)) => (Value::Undefined, false, Some(reason)),
        };
    engine_call_trust_method(
        engine,
        "finishTimerTask",
        &[selected, callback_error, Value::Bool(callback_failed)],
    )?;
    if let Some(reason) = callback_interrupted {
        return Err(EvalError::Interrupted(reason));
    }
    Ok(true)
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

    let mut timer_turns = 0usize;
    loop {
        let deadline = engine_call_trust_method(&mut engine, "nextDeadline", &[])
            .map_err(|error| describe_eval_error(&mut engine, error, "__trust.nextDeadline"))?;
        let Value::Num(deadline) = deadline else {
            break;
        };
        let ran = dispatch_timer_task_to(&mut engine, deadline)
            .map_err(|error| describe_eval_error(&mut engine, error, "timer task"))?;
        engine
            .run_microtasks_interruptible()
            .map_err(|reason| format!("timer task microtasks interrupted: {}", reason.message()))?;
        if ran {
            timer_turns += 1;
        } else {
            break;
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

#[cfg(feature = "lumen-backend")]
mod desktop {
    use super::*;
    use crate::js::{FormSubmission, Outcome, PageCmd, PageEnv, PageEvt, PageHandle, PageHover};
    use std::collections::HashSet;

    const PAGE_STACK: usize = 64 * 1024 * 1024;
    const HOST_TASK_RENDER_BURST: usize = 64;
    /// WHATWG HTML "update the rendering" is a rendering task selected at a
    /// hardware-constrained rendering opportunity, not the tail of every
    /// ordinary task. Model a foreground 60 Hz display; if rendering is slow,
    /// re-arming from the next pending update naturally drops missed frames.
    const RENDER_INTERVAL: Duration = Duration::from_micros(16_667);

    /// Run the Lumen page pipeline as a one-shot transformation.
    ///
    /// This path exists for diagnostics and the non-resident HTTP helpers. It
    /// uses the same parser task, microtask checkpoints, platform task source,
    /// timer ordering, and host-completion dispatch as the resident actor; the
    /// only difference is that virtual time advances to the next timer because
    /// there is no displayed document which could observe real time passing.
    pub(crate) fn transform(html: &str, env: &PageEnv) -> (String, Outcome) {
        let interrupt = Arc::new(lumen::RuntimeInterrupt::default());
        let (host_tx, mut host_rx) = tokio::sync::mpsc::unbounded_channel();
        let env = PageEnv {
            url: env.url.clone(),
            viewport: env.viewport,
            cell_px: env.cell_px,
            device_pixel_ratio: env.device_pixel_ratio,
            externals: env.externals.clone(),
            sheets: env.sheets.clone(),
            cache: env.cache.clone(),
            net: env.net.clone(),
            storage: env.storage.clone(),
            blobs: env.blobs.clone(),
        };
        let mut page = match load_page(html, env, host_tx, interrupt.clone()) {
            Ok(page) => page,
            Err(mut outcome) => {
                outcome.elapsed = Duration::ZERO;
                return (html.to_string(), outcome);
            }
        };
        let _ = evaluate_task(&mut page, "__trust.oneShot = true;", "one-shot setup");
        let _ = evaluate_task(
            &mut page,
            "__trust.readyState = 'complete'; __trust.fire(window, 'load', false);",
            "load event",
        );
        checkpoint(&mut page, "load event");

        // A one-shot document has no future rendering opportunity, so advance
        // its task sources until they are quiescent. The cap is a diagnostic
        // resource envelope, not a browsing deadline or replacement for the
        // resident event loop.
        for _ in 0..100_000 {
            if let Ok(task) = host_rx.try_recv() {
                if let Err(error) = dispatch_host_task(&mut page.engine, task) {
                    page.outcome.errors.push(error);
                }
                checkpoint(&mut page, "host task");
                continue;
            }
            if trust_bool(&mut page, "hasPlatformTask") {
                let ran = call_trust(&mut page, "runPlatformTask", &[], "platform task")
                    .is_some_and(|value| page.engine.ctx().to_boolean(&value));
                checkpoint(&mut page, "platform task");
                if ran {
                    continue;
                }
            }
            let Some(deadline) = trust_number(&mut page, "nextDeadline") else {
                break;
            };
            let ran = dispatch_timer_task(&mut page, deadline, "timer task");
            checkpoint(&mut page, "timer task");
            if !ran {
                break;
            }
        }
        if trust_bool(&mut page, "hasRenderingUpdate") {
            let _ = render_with_observers(&mut page);
        }
        page.outcome.elapsed = page.started.elapsed();
        let output = page.dom.borrow().serialize(DOCUMENT);
        (output, std::mem::take(&mut page.outcome))
    }

    struct ActorTaskTrace {
        interval_started: Instant,
        turns: u64,
        interactions: u64,
        commands: u64,
        hovers: u64,
        host_tasks: u64,
        platform_turns: u64,
        platform_tasks: u64,
        idle_turns: u64,
        timer_turns: u64,
        timer_tasks: u64,
        lifecycle_turns: u64,
        finishes: u64,
        dirty_finishes: u64,
        render_passes: u64,
        visual_updates: u64,
    }

    impl ActorTaskTrace {
        fn enabled() -> Option<Self> {
            std::env::var_os("TRUST_LUMEN_TASK_TRACE").map(|_| Self {
                interval_started: Instant::now(),
                turns: 0,
                interactions: 0,
                commands: 0,
                hovers: 0,
                host_tasks: 0,
                platform_turns: 0,
                platform_tasks: 0,
                idle_turns: 0,
                timer_turns: 0,
                timer_tasks: 0,
                lifecycle_turns: 0,
                finishes: 0,
                dirty_finishes: 0,
                render_passes: 0,
                visual_updates: 0,
            })
        }

        fn reset(&mut self) {
            self.interval_started = Instant::now();
            self.turns = 0;
            self.interactions = 0;
            self.commands = 0;
            self.hovers = 0;
            self.host_tasks = 0;
            self.platform_turns = 0;
            self.platform_tasks = 0;
            self.idle_turns = 0;
            self.timer_turns = 0;
            self.timer_tasks = 0;
            self.lifecycle_turns = 0;
            self.finishes = 0;
            self.dirty_finishes = 0;
            self.render_passes = 0;
            self.visual_updates = 0;
        }
    }

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
        /// DOM/environment changes and observer registrations awaiting HTML's
        /// next rendering opportunity. Ordinary task completion only sets this
        /// bit; it never runs style/layout or rendering observers itself.
        render_pending: bool,
        task_trace: Option<ActorTaskTrace>,
    }

    enum Wake {
        Interaction(Option<PageCmd>),
        Cmd(Option<PageCmd>),
        Hover(Option<PageHover>),
        Host(Option<LumenHostTask>),
        Platform,
        Idle(f64),
        Timer,
        Render,
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

    /// Spawn the production Lumen resident realm behind the actor contract
    /// shared by the terminal and desktop frontends.
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
        // HTML leaves selection among task queues to the user agent. Alternate a ready normal
        // browser-command lane with page-owned work so neither a continuously due timer nor a
        // continuously replenished frontend queue can exclude the other task sources.
        let mut prefer_command = true;

        // A truly inert document does not need a resident realm. Complete its load task first so
        // a load handler can still create controls, timers, workers, observers, or navigation;
        // only then classify the final state as Static. Interactive or pending-work pages retain
        // the ordinary shell-before-load rendering opportunity below.
        if !has_resident_work(&mut page, has_interaction) {
            prepare_unbounded_task(&interrupt);
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
        let mut deferred_host_task = None;
        let mut render_deadline = None;

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
            let timer_due = deadline.is_some_and(|deadline| deadline <= now);
            if trust_bool(&mut page, "hasRenderingUpdate") {
                page.render_pending = true;
            }
            if page.render_pending && render_deadline.is_none() {
                render_deadline = Some(Instant::now() + RENDER_INTERVAL);
            }
            let render_due = render_deadline.is_some_and(|deadline| deadline <= Instant::now());
            let platform_ready = trust_bool(&mut page, "hasPlatformTask");
            let idle_deadline = if !platform_ready && trust_bool(&mut page, "hasIdleRequest") {
                let end = deadline.map_or(now + 50.0, |deadline| deadline.min(now + 50.0));
                (end > now).then_some(end)
            } else {
                None
            };
            let load_ready = !lifecycle_complete
                && pending_resources(&mut page) == 0
                && !trust_bool(&mut page, "hasInitialFramesPending");

            let mut immediate = None;
            if let Ok(command) = interactions.try_recv() {
                immediate = Some(Wake::Interaction(Some(command)));
            } else if hover.has_changed().unwrap_or(false) {
                immediate = Some(Wake::Hover(Some(*hover.borrow_and_update())));
            // HTML §8.1.7 lets a user agent choose among runnable task queues while preserving
            // FIFO order within each task source. Browser-state changes such as a viewport resize
            // must be selected promptly: putting this bounded queue behind every already-due
            // author timer lets a busy page starve its own resize task forever.
            } else if prefer_command && let Ok(command) = cmds.try_recv() {
                immediate = Some(Wake::Cmd(Some(command)));
            } else if render_due {
                immediate = Some(Wake::Render);
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
                // No page-owned source was runnable, so drain the command lane
                // without manufacturing an idle turn solely for alternation.
                immediate = Some(Wake::Cmd(Some(command)));
            } else if let Some(deadline) = idle_deadline {
                immediate = Some(Wake::Idle(deadline));
            }

            let timer_wait = deadline
                .map(|deadline| Duration::from_secs_f64(((deadline - now).max(0.0)) / 1000.0));
            let render_wait =
                render_deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let (wait, timeout_wake) = match (timer_wait, render_wait) {
                (Some(timer), Some(render)) if render < timer => (Some(render), Wake::Render),
                (Some(timer), _) => (Some(timer), Wake::Timer),
                (None, Some(render)) => (Some(render), Wake::Render),
                (None, None) => (None, Wake::Timer),
            };
            let wake = immediate.unwrap_or_else(|| {
                // Lumen's forced host collection is an idle hook, not a task-
                // boundary hook. A future timer/animation-frame deadline means
                // the realm still has active work; allocation-triggered task
                // checks retain responsibility there. Forcing a full tracing
                // collection before every 16 ms animation sleep made a large
                // YouTube heap consume a core while callbacks made no DOM
                // changes. Collect only before a genuinely indefinite park.
                if wait.is_none() {
                    page.engine.collect_garbage_at_idle();
                }
                interrupt.set_deadline(None);
                runtime.block_on(async {
                    tokio::select! {
                        biased;
                        command = interactions.recv() => Wake::Interaction(command),
                        changed = hover.changed() => Wake::Hover(changed.ok().map(|()| *hover.borrow_and_update())),
                        task = host_rx.recv() => Wake::Host(task),
                        command = cmds.recv() => Wake::Cmd(command),
                        () = sleep_or_pending(wait) => timeout_wake,
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

            if let Some(trace) = page.task_trace.as_mut() {
                trace.turns += 1;
                match &wake {
                    Wake::Interaction(_) => trace.interactions += 1,
                    Wake::Cmd(_) => trace.commands += 1,
                    Wake::Hover(_) => trace.hovers += 1,
                    Wake::Host(_) => {}
                    Wake::Platform => trace.platform_turns += 1,
                    Wake::Idle(_) => trace.idle_turns += 1,
                    Wake::Timer => trace.timer_turns += 1,
                    Wake::Render => {}
                    Wake::Lifecycle => trace.lifecycle_turns += 1,
                }
            }

            match wake {
                Wake::Interaction(Some(command)) | Wake::Cmd(Some(command)) => {
                    if !dispatch_command(&mut page, command, &events, &interrupt) {
                        break;
                    }
                    prefer_timer = true;
                    prefer_command = false;
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
                        if let Some(trace) = page.task_trace.as_mut() {
                            trace.host_tasks += 1;
                        }
                        prepare_unbounded_task(&interrupt);
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
                    if !finish_internal_task(&mut page, &events) {
                        break;
                    }
                    prefer_timer = true;
                    prefer_command = true;
                }
                Wake::Host(None) => break,
                Wake::Platform => {
                    prepare_unbounded_task(&interrupt);
                    if page.task_trace.is_some() {
                        let queues = call_trust(&mut page, "taskQueueState", &[], "task trace")
                            .map(|value| value_string(&mut page.engine, &value))
                            .unwrap_or_else(|| String::from("unavailable"));
                        eprintln!("lumen: platform task begin queues={queues}");
                    }
                    let ran = call_trust(&mut page, "runPlatformTask", &[], "platform task")
                        .is_some_and(|value| page.engine.ctx().to_boolean(&value));
                    if page.task_trace.is_some() {
                        eprintln!("lumen: platform task end ran={ran}");
                    }
                    if ran && let Some(trace) = page.task_trace.as_mut() {
                        trace.platform_tasks += 1;
                    }
                    checkpoint(&mut page, "platform task");
                    if !finish_internal_task(&mut page, &events) {
                        break;
                    }
                    prefer_timer = true;
                    prefer_command = true;
                }
                Wake::Idle(deadline) => {
                    prepare_unbounded_task(&interrupt);
                    let _ = call_trust(
                        &mut page,
                        "startIdlePeriod",
                        &[Value::Num(deadline)],
                        "start idle period",
                    );
                    let ran = call_trust(&mut page, "runPlatformTask", &[], "idle task")
                        .is_some_and(|value| page.engine.ctx().to_boolean(&value));
                    if ran && let Some(trace) = page.task_trace.as_mut() {
                        trace.platform_tasks += 1;
                    }
                    checkpoint(&mut page, "idle task");
                    if !finish_internal_task(&mut page, &events) {
                        break;
                    }
                    prefer_timer = true;
                    prefer_command = true;
                }
                Wake::Timer => {
                    prepare_unbounded_task(&interrupt);
                    let real_now = virtual_origin + wall_origin.elapsed().as_secs_f64() * 1000.0;
                    let animation_frame = trust_bool(&mut page, "nextDeadlineIsAnimationFrame");
                    if page.task_trace.is_some() {
                        let queues = call_trust(&mut page, "taskQueueState", &[], "task trace")
                            .map(|value| value_string(&mut page.engine, &value))
                            .unwrap_or_else(|| String::from("unavailable"));
                        eprintln!(
                            "lumen: timer task begin deadline={real_now:.3} animation_frame={animation_frame} queues={queues}"
                        );
                    }
                    let timer_started = Instant::now();
                    let ran = dispatch_timer_task(&mut page, real_now, "timer task");
                    if page.task_trace.is_some() {
                        eprintln!(
                            "lumen: timer task end ran={ran} elapsed={:.3}s",
                            timer_started.elapsed().as_secs_f64()
                        );
                    }
                    if ran && let Some(trace) = page.task_trace.as_mut() {
                        trace.timer_tasks += 1;
                    }
                    checkpoint(&mut page, "timer task");
                    // requestAnimationFrame callbacks are step 14 of the same
                    // HTML rendering update whose style/layout and observer
                    // steps follow them. Ordinary timer tasks merely request a
                    // future rendering opportunity when they mutate the page.
                    let finished = if animation_frame {
                        finish_task_with_ack(&mut page, &events, false)
                    } else {
                        finish_internal_task(&mut page, &events)
                    };
                    if !finished {
                        break;
                    }
                    prefer_timer = false;
                    prefer_command = true;
                }
                Wake::Lifecycle => {
                    lifecycle_complete = true;
                    prepare_unbounded_task(&interrupt);
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
                    if !finish_internal_task(&mut page, &events) {
                        break 'event_loop;
                    }
                    prefer_timer = true;
                    prefer_command = true;
                }
                Wake::Render => {
                    render_deadline = None;
                    if !finish_task_with_ack(&mut page, &events, false) {
                        break;
                    }
                    prefer_timer = true;
                    prefer_command = true;
                }
            }
            if !page.render_pending {
                render_deadline = None;
            }
            report_task_trace(&mut page);
            // Ordinary tasks have no wall-clock deadline. Clear an explicit
            // diagnostic deadline before the host returns to scheduling;
            // navigation and cancellation use separate interrupt state.
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

        interrupt.set_deadline(None);
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
            state.enable_network(
                response_url.clone(),
                handle,
                env.cache.clone(),
                host_tasks.clone(),
            );
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
        // User-Agent value. Keep that identical to the HTTP client and the
        // selected JS realm; the engine implementation is not a distinct
        // user agent or an observable browser capability.
        // WHATWG HTML §2.4.3 keeps a Document's URL distinct from its
        // document base URL. A <base href> changes relative-URL resolution
        // and Node.baseURI, but it must not rewrite Location or document.URL.
        // SPA shells commonly serve the same markup at every route and select
        // the route from location.pathname, so seeding the realm with `base`
        // collapses every such navigation to the base path.
        let config = format!(
            "globalThis.__trust_cfg = {{ url: {}, ua: 'TRust/0.1', language: {}, languages: [{}, {}], width: {}, height: {}, devicePixelRatio: {}, hardwareConcurrency: {}, globalPrivacyControl: {}, secureContext: {} }};",
            json_string(response_url.as_str()),
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
            lumen_potentially_trustworthy(&response_url),
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
                let source = initial_classic_source(src.as_deref(), &inline, &env, &base);
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
            render_pending: false,
            task_trace: ActorTaskTrace::enabled(),
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
        base: &url::Url,
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
        if let Some(body) = env
            .externals
            .iter()
            .find(|(name, _)| name == src)
            .and_then(|(_, body)| body.as_ref())
        {
            return Some((
                src.to_string(),
                String::from_utf8_lossy(body).into_owned(),
                true,
            ));
        }

        // HTML §4.12.1.1, "prepare the script element": prefetching is an
        // optional optimization. Once a connected classic script with `src`
        // is prepared, fetching that classic script is mandatory even when a
        // preload scanner did not announce it.
        let resolved = base.join(src).ok()?;
        let handle = env.net.as_ref()?;
        let fetch = env
            .cache
            .peek(&resolved)
            .unwrap_or_else(|| env.cache.fetch(handle, resolved.clone()));
        let response = crate::http::PageCache::block_on_fetch(Some(handle), fetch)?;
        crate::http::classic_script_response_allowed(
            response.status,
            &response.content_type,
            &response.headers,
        )
        .then(|| {
            (
                resolved.to_string(),
                crate::http::decode_body(&response.content_type, &response.body),
                true,
            )
        })
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
            || trust_bool(page, "hasIdleRequest")
            || trust_bool(page, "hasScrollWork")
            || trust_bool(page, "hasResizeObserver")
            || trust_bool(page, "hasInitialFramesPending")
    }

    /// HTML §8.1.7.3 runs an ordinary selected task to completion before the
    /// microtask checkpoint. Ordinary page tasks do not have a host-imposed
    /// wall-clock deadline; navigation and document teardown use their own
    /// explicit interrupt paths.
    fn prepare_unbounded_task(interrupt: &Arc<lumen::RuntimeInterrupt>) {
        interrupt.set_deadline(None);
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

    fn dispatch_timer_task(page: &mut LumenPage, deadline: f64, label: &str) -> bool {
        let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_timer_task_to(&mut page.engine, deadline)
        }));
        match dispatched {
            Ok(Ok(ran)) => ran,
            Ok(Err(error)) => {
                record_eval_error(page, error, label);
                false
            }
            Err(_) => {
                page.outcome
                    .errors
                    .push(format!("{label}: Lumen engine panic — page JS halted"));
                page.outcome.panicked = true;
                false
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
        let started = Instant::now();
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
        if page.task_trace.is_some() && started.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "lumen: {label} checkpoint elapsed={:.3}s",
                started.elapsed().as_secs_f64()
            );
        }
    }

    fn drain_diagnostics(page: &mut LumenPage) {
        let error_start = page.outcome.errors.len();
        let console_start = page.outcome.console.len();
        for (source, errors) in [
            ("__trust.takeErrors()", true),
            ("__trust.takeLogs()", false),
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

    fn report_task_trace(page: &mut LumenPage) {
        let due = page
            .task_trace
            .as_ref()
            .is_some_and(|trace| trace.interval_started.elapsed() >= Duration::from_secs(1));
        if !due {
            return;
        }
        let queues = call_trust(page, "taskQueueState", &[], "task trace")
            .map(|value| value_string(&mut page.engine, &value))
            .unwrap_or_else(|| String::from("unavailable"));
        let Some(trace) = page.task_trace.as_mut() else {
            return;
        };
        eprintln!(
            "lumen: tasks {:.3}s turns={} interaction={} cmd={} hover={} host={} platform={}/{} idle={} timer={}/{} lifecycle={} finish={} dirty={} render={} updated={} queues={}",
            trace.interval_started.elapsed().as_secs_f64(),
            trace.turns,
            trace.interactions,
            trace.commands,
            trace.hovers,
            trace.host_tasks,
            trace.platform_tasks,
            trace.platform_turns,
            trace.idle_turns,
            trace.timer_tasks,
            trace.timer_turns,
            trace.lifecycle_turns,
            trace.finishes,
            trace.dirty_finishes,
            trace.render_passes,
            trace.visual_updates,
            queues,
        );
        trace.reset();
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
    /// HTML's rendering loop synchronously repeats style/layout only for active
    /// ResizeObserver broadcasts. Intersection Observer §3.2.4 instead queues
    /// notification on its own task source after recording intersections; its
    /// callbacks cannot force another layout inside this rendering opportunity.
    fn render_with_observers(page: &mut LumenPage) -> (String, crate::http::RenderedPage, bool) {
        let mut rendered = extract_live(page);
        // CSSOM View §13.1 compares each nested Document's viewport after the
        // layout pass that established its iframe dimensions. The parent
        // resize task updates the top-level viewport first; run the nested
        // resize steps here, then repaint if a child handler changed the DOM.
        if trust_number(page, "updateFrameResizes").unwrap_or(0.0) > 0.0 {
            rendered = extract_live(page);
        }
        for _ in 0..6 {
            let resized = trust_number(page, "updateResizes").unwrap_or(0.0);
            checkpoint(page, "rendering observers");
            if resized <= 0.0 || page.outcome.panicked {
                break;
            }
            rendered = extract_live(page);
        }
        let _ = trust_number(page, "updateIntersections");
        rendered
    }

    /// SVG 2 §5.6: a same-origin external `<use href="sheet.svg#symbol">` obtains the external
    /// resource document before the use-element shadow tree can be rendered. Keep the resource
    /// cache and request policy identical to the other required Lumen subresource paths.
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
                    network
                        .fetched
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                prepare_unbounded_task(interrupt);
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
                prepare_unbounded_task(interrupt);
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
                        state.geom_cache.borrow_mut().epoch = u64::MAX;
                    }
                }
                if changed {
                    page.render_environment_dirty = true;
                    prepare_unbounded_task(interrupt);
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
                        state.geom_cache.borrow_mut().epoch = u64::MAX;
                        true
                    });
                page.dom
                    .borrow_mut()
                    .set_viewport_px(viewport.width, viewport.height);
                if changed {
                    page.render_environment_dirty = true;
                    prepare_unbounded_task(interrupt);
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
                        state.geom_cache.borrow_mut().epoch = u64::MAX;
                        true
                    });
                page.dom.borrow_mut().set_device_pixel_ratio(ratio);
                if changed {
                    page.render_environment_dirty = true;
                    prepare_unbounded_task(interrupt);
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
        prepare_unbounded_task(interrupt);
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
    /// complete typed layouts. This follows the same conservative rule as the other actor: every
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
        finish_task_maybe_render(page, events, acknowledge_settle, true)
    }

    fn finish_internal_task(
        page: &mut LumenPage,
        events: &tokio::sync::mpsc::Sender<PageEvt>,
    ) -> bool {
        finish_task_maybe_render(page, events, false, false)
    }

    /// Complete one HTML event-loop task. History/navigation/form/error side
    /// effects are observable at task completion, but HTML §8.1.7.3 queues
    /// "update the rendering" on its distinct rendering task source. Only an
    /// actual rendering opportunity (or a browser interaction that needs an
    /// immediate presentation acknowledgement) consumes `render_pending`.
    fn finish_task_maybe_render(
        page: &mut LumenPage,
        events: &tokio::sync::mpsc::Sender<PageEvt>,
        acknowledge_settle: bool,
        render_now: bool,
    ) -> bool {
        if page.outcome.panicked {
            let errors = std::mem::take(&mut page.outcome.errors);
            let _ = events.blocking_send(PageEvt::Trouble(errors));
            return false;
        }
        for (url, replace) in take_history_updates(page) {
            if events
                .blocking_send(PageEvt::HistoryUpdate { url, replace })
                .is_err()
            {
                return false;
            }
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
        page.render_pending |= dom_dirty || environment_dirty;
        if let Some(trace) = page.task_trace.as_mut() {
            trace.finishes += 1;
            if dom_dirty || environment_dirty {
                trace.dirty_finishes += 1;
            }
        }
        #[cfg(test)]
        let mut render_handled = false;
        #[cfg(not(test))]
        let render_handled = false;
        #[cfg(test)]
        if render_now
            && page.render_pending
            && dom_dirty
            && !environment_dirty
            // The ignored public-site acceptance gate must exercise the same
            // complete typed-render path as release binaries. Boundary patches
            // are a test-only protocol used by focused actor tests.
            && std::env::var_os("TRUST_BROWSER_GATE").is_none()
        {
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
        if render_handled {
            page.render_pending = false;
        }
        if render_now && page.render_pending && !render_handled {
            if let Some(trace) = page.task_trace.as_mut() {
                trace.render_passes += 1;
            }
            let render_started = Instant::now();
            let (html, rendered, _) = render_with_observers(page);
            if page.task_trace.is_some() && render_started.elapsed() >= Duration::from_secs(1) {
                eprintln!(
                    "lumen: rendering update elapsed={:.3}s boxes={} html={}B",
                    render_started.elapsed().as_secs_f64(),
                    rendered.layout.boxes.len(),
                    html.len()
                );
            }
            page.render_pending = false;
            let presentation_changed = page
                .last_render
                .as_ref()
                .is_none_or(|previous| !previous.visually_eq(&rendered));
            #[cfg(test)]
            let diagnostic_changed = std::env::var_os("TRUST_BROWSER_GATE").is_none()
                && page
                    .last_diagnostic_render
                    .as_deref()
                    .is_none_or(|previous| previous != crate::js::render_canonical(&html));
            #[cfg(not(test))]
            let diagnostic_changed = false;
            let changed = presentation_changed || diagnostic_changed;
            if changed {
                if let Some(trace) = page.task_trace.as_mut() {
                    trace.visual_updates += 1;
                }
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

    fn take_history_updates(page: &mut LumenPage) -> Vec<(String, bool)> {
        let Some(value) = call_trust(page, "takeHistoryUpdates", &[], "history update") else {
            return Vec::new();
        };
        let Some(json) = value_to_string(page, &value) else {
            return Vec::new();
        };
        crate::js::decode_history_updates(&json)
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
        async fn actor_async_request_submit_preserves_hidden_successful_controls() {
            // WHATWG HTML §§4.10.22.3–4.10.22.4 and
            // HTMLFormElement.requestSubmit(): mirror Reddit's verification
            // document. An async DOMContentLoaded handler mutates a hidden
            // named control, then no-argument requestSubmit() constructs the
            // entry list without selecting an arbitrary submit button.
            let html = r#"<!doctype html><html><body>
                <main>Please wait</main>
                <form action="/verify" method="get" hidden>
                    <input type="hidden" name="solution" value="0">
                    <input type="hidden" name="token" value="abc">
                    <button name="go" value="chosen">Continue</button>
                </form>
                <script>
                    document.addEventListener("DOMContentLoaded", async function () {
                        const form = document.forms[0];
                        const answer = await (async value => value + value)(21);
                        form.elements.namedItem("solution").value = answer;
                        form.requestSubmit();
                    }, { once: true });
                </script>
            </body></html>"#;
            let (_handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));
            let submission = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { outcome, .. }) => {
                            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                        }
                        Some(PageEvt::SubmitForm {
                            submitter,
                            submission,
                            ..
                        }) => {
                            assert_eq!(
                                submitter, None,
                                "no-argument requestSubmit has no submitter"
                            );
                            break submission.expect("entry list built in the live DOM");
                        }
                        Some(PageEvt::Trouble(errors)) => {
                            panic!("requestSubmit actor errors: {errors:?}")
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before requestSubmit navigation"),
                    }
                }
            })
            .await
            .expect("async requestSubmit timed out");

            assert_eq!(submission.action, "https://example.com/verify");
            assert_eq!(submission.method, "get");
            assert_eq!(submission.body, "solution=42&token=abc");
        }

        #[tokio::test]
        async fn actor_coalesces_ordinary_tasks_before_resize_observer_rendering() {
            // WHATWG HTML §8.1.7.3 selects rendering from its own task source,
            // and Resize Observer §3.4 runs inside that rendering update. Two
            // already-runnable port-message tasks may therefore be coalesced;
            // the observer must not expose the intermediate box size merely
            // because one ordinary task completed.
            let html = r#"<!doctype html><html><body>
                <div id="box" style="width:10px;height:10px"></div>
                <output id="result"></output>
                <script>
                    const box = document.getElementById("box");
                    const result = document.getElementById("result");
                    const seen = [];
                    new ResizeObserver(function (entries) {
                        seen.push(Math.round(entries[0].contentRect.width));
                        result.textContent = seen.join(",");
                    }).observe(box);
                    const channel = new MessageChannel();
                    channel.port1.onmessage = function (event) {
                        box.style.width = event.data + "px";
                    };
                    channel.port2.postMessage(20);
                    channel.port2.postMessage(30);
                </script>
            </body></html>"#;
            let (_handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));

            let mut exposed_intermediate_size = false;
            let final_render = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, .. }) => {
                            exposed_intermediate_size |= html.contains(">10,20</output>");
                            if html.contains(">10,30</output>") {
                                break html;
                            }
                        }
                        Some(PageEvt::Trouble(errors)) => {
                            panic!("rendering-opportunity fixture failed: {errors:?}")
                        }
                        Some(_) => {}
                        None => panic!("actor closed before its rendering opportunity"),
                    }
                }
            })
            .await
            .expect("coalesced rendering opportunity timed out");

            assert!(!exposed_intermediate_size, "{final_render}");
        }

        #[tokio::test]
        async fn actor_navigates_slotted_link_descendant_activation() {
            // DOM §2.9 + HTML links: the painted image is the click target,
            // while the enclosing anchor in a closed shadow tree is the
            // activation target selected from its composed event path. This is
            // the structure used by archive.org's collection item tiles.
            let html = r#"<!doctype html><html><body>
                <x-tile id="tile"><img id="target" alt="item"></x-tile>
                <script>
                    const root = document.getElementById("tile")
                        .attachShadow({ mode: "closed" });
                    root.innerHTML = '<a href="/details/vhskids"><slot></slot></a>';
                    document.getElementById("target").addEventListener("click", () => {});
                </script>
            </body></html>"#;
            let target = Dom::parse_document(html).get_by_id("target").unwrap();
            let (handle, mut events) = spawn_page(
                html.to_string(),
                PageEnv::bare("https://archive.org/details/vhsvault"),
            );
            tokio::time::timeout(Duration::from_secs(30), async {
                match events.recv().await {
                    Some(PageEvt::Updated { .. }) => {}
                    Some(PageEvt::Trouble(errors)) => {
                        panic!("initial page task failed: {errors:?}")
                    }
                    Some(other) => panic!("expected live page update, got {other:?}"),
                    None => panic!("Lumen actor closed before initial render"),
                }
            })
            .await
            .expect("initial Lumen render timed out");

            handle.try_send_navigation_click(target).unwrap();
            let navigated = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Navigate(url)) => break url,
                        Some(PageEvt::Trouble(errors)) => panic!("click task failed: {errors:?}"),
                        Some(_) => {}
                        None => panic!("Lumen actor closed during slotted link click"),
                    }
                }
            })
            .await
            .expect("slotted hyperlink navigation timed out");
            assert_eq!(navigated, "https://archive.org/details/vhskids");
        }

        #[tokio::test]
        async fn parser_document_write_uses_the_current_script_insertion_point() {
            // WHATWG HTML §8.4.3 inserts each document.write string immediately
            // before the active parser insertion point. The source is already a
            // DOM when TRust executes parser scripts, so the equivalent cursor is
            // after the script and before its original following sibling.
            let html = r#"<!doctype html><html><body>
                <div id="contentW"><span id="before">before</span><script>
                    document.write('written-text<i id="first">first</i>');
                    document.write('<b id="second">second</b>');
                </script><em id="after">after</em></div>
                <div id="body-tail">tail</div>
            </body></html>"#;
            let (_handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));
            let rendered = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, .. } | PageEvt::Static { html, .. })
                            if html.contains("id=\"second\"") =>
                        {
                            break html;
                        }
                        Some(PageEvt::Trouble(errors)) => {
                            panic!("document.write failed: {errors:?}")
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before document.write rendered"),
                    }
                }
            })
            .await
            .expect("parser document.write render timed out");

            let content = rendered
                .split_once("<div id=\"contentW\">")
                .and_then(|(_, rest)| rest.split_once("</div>"))
                .map(|(content, _)| content)
                .expect("serialized contentW");
            let before = content.find("id=\"before\"").expect("source predecessor");
            let text = content.find("written-text").expect("written text");
            let first = content.find("id=\"first\"").expect("first write");
            let second = content.find("id=\"second\"").expect("second write");
            let after = content.find("id=\"after\"").expect("source successor");
            assert!(
                before < text && text < first && first < second && second < after,
                "{content}"
            );
            assert!(
                rendered.find("id=\"second\"").unwrap()
                    < rendered.find("id=\"body-tail\"").unwrap(),
                "written nodes escaped their parser parent: {rendered}"
            );
        }

        #[tokio::test]
        async fn actor_does_not_abort_a_long_running_click_task_by_wall_clock() {
            // HTML §8.1.7.3 runs the selected user-interaction task to
            // completion. A user agent can expose an explicit "stop script"
            // intervention, but an arbitrary one-second host deadline must not
            // silently interrupt a valid handler. YouTube's SPA click handler
            // exceeds one second under the interpreter before it publishes its
            // /watch navigation.
            let html = r#"<!doctype html><html><body>
                <a id="target" href="https://www.youtube.com/watch?v=standard">watch</a>
                <script>
                    document.getElementById("target").addEventListener("click", function (event) {
                        event.preventDefault();
                        const started = performance.now();
                        while (performance.now() - started < 1200) {}
                        location.href = this.href;
                    });
                </script>
            </body></html>"#;
            let target = Dom::parse_document(html).get_by_id("target").unwrap();
            let (handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if matches!(events.recv().await, Some(PageEvt::Updated { .. })) {
                        break;
                    }
                }
            })
            .await
            .expect("initial Lumen render timed out");

            handle.try_send_user(PageCmd::Click(target)).unwrap();
            let navigated = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Navigate(url)) => break url,
                        Some(PageEvt::Trouble(errors)) => panic!("click task failed: {errors:?}"),
                        Some(_) => {}
                        None => panic!("Lumen actor closed during click task"),
                    }
                }
            })
            .await
            .expect("long-running click task was wall-clock interrupted");
            assert_eq!(
                navigated, "https://www.youtube.com/watch?v=standard",
                "click handler must publish its navigation after completing"
            );
            drop(handle);
        }

        #[tokio::test]
        async fn actor_reports_spa_history_urls_without_cross_document_navigation() {
            // YouTube's anchor handler cancels the default navigation and then
            // commits /watch with history.pushState(). HTML makes this a
            // same-document URL/history update: the host must observe it, but
            // must not fetch or discard the resident realm.
            let html = r#"<!doctype html><html><body>
                <a id="target" href="https://www.youtube.com/watch?v=spa">watch</a>
                <script>
                    document.getElementById("target").addEventListener("click", function (event) {
                        event.preventDefault();
                        history.pushState({ video: "spa" }, "", this.href);
                    });
                </script>
            </body></html>"#;
            let target = Dom::parse_document(html).get_by_id("target").unwrap();
            let (handle, mut events) = spawn_page(
                html.to_string(),
                PageEnv::bare("https://www.youtube.com/results?search_query=spa"),
            );
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if matches!(events.recv().await, Some(PageEvt::Updated { .. })) {
                        break;
                    }
                }
            })
            .await
            .expect("initial Lumen render timed out");

            handle.try_send_user(PageCmd::Click(target)).unwrap();
            let (url, replace) = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::HistoryUpdate { url, replace }) => break (url, replace),
                        Some(PageEvt::Navigate(url) | PageEvt::Replace(url)) => {
                            panic!("pushState became a document navigation: {url}")
                        }
                        Some(PageEvt::Trouble(errors)) => panic!("click task failed: {errors:?}"),
                        Some(_) => {}
                        None => panic!("Lumen actor closed during SPA click"),
                    }
                }
            })
            .await
            .expect("same-document history update timed out");
            assert_eq!(url, "https://www.youtube.com/watch?v=spa");
            assert!(!replace);
            assert!(handle.try_send_user(PageCmd::Click(target)).is_ok());
        }

        #[tokio::test]
        async fn actor_keeps_document_url_distinct_from_base_url() {
            // WHATWG HTML §2.4.3: the first <base href> supplies the document
            // base URL, while the Document's URL remains the navigation URL.
            // Client-side routers select a route from Location, so replacing
            // it with the base URL makes every route mount the root page.
            let html = r#"<!doctype html><html><head><base href="/"></head><body>
                <output id="result"></output>
                <script>
                    document.getElementById("result").textContent = [
                        location.href,
                        location.pathname,
                        document.URL,
                        document.baseURI
                    ].join("|");
                </script>
            </body></html>"#;
            let route = "https://example.test/details/collection?tab=items";
            let (_handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(route));

            let rendered = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, .. } | PageEvt::Static { html, .. })
                            if html.contains("id=\"result\"") =>
                        {
                            break html;
                        }
                        Some(PageEvt::Trouble(errors)) => {
                            panic!("document URL test failed: {errors:?}")
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before rendering the document URL"),
                    }
                }
            })
            .await
            .expect("document URL render timed out");

            assert!(
                rendered.contains(
                    "https://example.test/details/collection?tab=items|/details/collection|https://example.test/details/collection?tab=items|https://example.test/"
                ),
                "Document URL and base URL were not kept distinct: {rendered}"
            );
        }

        #[tokio::test]
        async fn actor_starts_idle_period_only_after_ordinary_tasks_quiesce() {
            let html = r#"<!doctype html><html><body>
                <output id="result">waiting</output>
                <script>
                    requestIdleCallback(function (deadline) {
                        document.getElementById("result").textContent =
                            "idle:" + deadline.didTimeout + ":" + (deadline.timeRemaining() > 0);
                    });
                </script>
            </body></html>"#;
            let (_handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));
            let rendered = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, .. }) if html.contains("idle:false:true") => {
                            break html;
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before its idle period"),
                    }
                }
            })
            .await
            .expect("idle callback did not receive an actor-selected idle period");
            assert!(rendered.contains("<output id=\"result\">idle:false:true</output>"));
        }

        #[tokio::test]
        async fn actor_does_not_starve_browser_commands_behind_due_timers() {
            // WHATWG HTML §8.1.7 permits the user agent to choose among task queues, while
            // preserving each task source's order. That scheduling freedom cannot make a
            // continuously due timer queue prevent the browser from ever updating the page's
            // viewport: real responsive applications depend on the resulting resize task.
            let html = r#"<!doctype html><html><body>
                <output id="result">waiting</output>
                <script>
                    for (let i = 0; i < 16; ++i) {
                        setInterval(function () {
                            const until = performance.now() + 20;
                            while (performance.now() < until) {}
                        }, 1);
                    }
                    window.addEventListener("resize", function () {
                        document.getElementById("result").textContent =
                            "resized:" + innerWidth + "x" + innerHeight;
                    });
                </script>
            </body></html>"#;
            let (handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if matches!(events.recv().await, Some(PageEvt::Updated { .. })) {
                        break;
                    }
                }
            })
            .await
            .expect("initial Lumen render timed out");

            // Let the interval become overdue before queuing the browser task. Each callback
            // takes longer than its repeat interval, so the timer queue remains continuously due.
            tokio::time::sleep(Duration::from_millis(100)).await;

            handle
                .cmds
                .send(PageCmd::Viewport(crate::layout2::Viewport::new(
                    400.0, 320.0,
                )))
                .await
                .unwrap();
            let resized = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, outcome })
                            if html.contains("resized:400x320") =>
                        {
                            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                            break;
                        }
                        Some(PageEvt::Trouble(errors)) => {
                            panic!("viewport command failed: {errors:?}")
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before viewport command"),
                    }
                }
            })
            .await;
            assert!(resized.is_ok(), "due timers starved the viewport task");
        }

        #[tokio::test]
        async fn actor_does_not_starve_due_timers_behind_browser_commands() {
            // The inverse fairness edge is equally important: prioritizing a browser-command
            // task source is not permission to exclude the timer task source indefinitely.
            // Queue the follow-up commands while the first handler is still running, after it
            // has made a zero-delay timer runnable. Observe its history update at task
            // completion rather than its DOM paint: HTML schedules that paint separately on
            // the rendering task source, so intervening commands may legitimately settle.
            let html = r#"<!doctype html><html><body>
                <button id="target">schedule</button>
                <output id="result">waiting</output>
                <script>
                    document.getElementById("target").addEventListener("click", function () {
                        setTimeout(function () {
                            document.getElementById("result").textContent = "timer-ran";
                            history.pushState(null, "", "?timer-ran");
                        }, 0);
                        const until = performance.now() + 100;
                        while (performance.now() < until) {}
                    });
                </script>
            </body></html>"#;
            let target = Dom::parse_document(html).get_by_id("target").unwrap();
            let (handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if matches!(events.recv().await, Some(PageEvt::Updated { .. })) {
                        break;
                    }
                }
            })
            .await
            .expect("initial Lumen render timed out");

            handle.cmds.send(PageCmd::Click(target)).await.unwrap();
            for _ in 0..16 {
                handle
                    .cmds
                    .send(PageCmd::SetScroll {
                        node: usize::MAX,
                        top: 0.0,
                        left: 0.0,
                    })
                    .await
                    .unwrap();
            }

            let settled_before_timer = tokio::time::timeout(Duration::from_secs(5), async {
                let mut settled = 0usize;
                loop {
                    match events.recv().await {
                        Some(PageEvt::HistoryUpdate { url, .. }) if url.ends_with("?timer-ran") => {
                            break settled;
                        }
                        Some(PageEvt::Settled) => settled += 1,
                        Some(PageEvt::Trouble(errors)) => {
                            panic!("timer fairness fixture failed: {errors:?}")
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before the timer task"),
                    }
                }
            })
            .await
            .expect("browser commands starved a runnable timer task");
            assert!(
                settled_before_timer < 16,
                "the actor drained the browser command queue before selecting the timer"
            );
        }

        #[tokio::test]
        async fn viewport_push_fires_resize_in_each_changed_frame_window() {
            // CSSOM View §13.1 runs resize steps for every Document whose
            // viewport changed, including an iframe resized because the top
            // viewport grew. The callback must execute with that frame's
            // document and inner size, not the top-level Window state shared
            // by the single-realm implementation.
            let html = r#"<!doctype html><html><body style="margin:0">
                <button onclick="void 0">keep</button><script>
                const frame = document.createElement('iframe');
                frame.id = 'frame';
                frame.style.cssText =
                    'position:fixed;inset:0;width:100%;height:100%;border:0';
                frame.srcdoc = '<body><output id="child">initial</output><scr' + 'ipt>' +
                    'window.addEventListener("resize", function () {' +
                    'document.getElementById("child").textContent = ' +
                    '"child " + innerWidth + "x" + innerHeight;' +
                    '});</scr' + 'ipt></body>';
                document.body.appendChild(frame);
                </script>
            </body></html>"#;
            let (handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));

            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, .. } | PageEvt::Static { html, .. })
                            if html.contains("id=\"child\"") =>
                        {
                            break;
                        }
                        Some(PageEvt::Trouble(errors)) => panic!("frame load failed: {errors:?}"),
                        Some(_) => {}
                        None => panic!("Lumen actor closed before the frame loaded"),
                    }
                }
            })
            .await
            .expect("initial frame render timed out");

            handle
                .cmds
                .send(PageCmd::Viewport(crate::layout2::Viewport::new(
                    400.0, 320.0,
                )))
                .await
                .unwrap();
            let resized = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, outcome })
                            if html.contains("child 400x320") =>
                        {
                            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                            break true;
                        }
                        Some(PageEvt::Trouble(errors)) => panic!("frame resize failed: {errors:?}"),
                        Some(_) => {}
                        None => break false,
                    }
                }
            })
            .await
            .expect("nested frame resize timed out");
            assert!(
                resized,
                "the changed child viewport must resize its own Window in frame scope"
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

        #[tokio::test]
        async fn iframe_load_waits_for_parser_module_and_dom_content_loaded() {
            // HTML §4.12.1.1 and §13.2.7: a parser-created module without `async`
            // executes after parsing and before DOMContentLoaded. The nested
            // document reaches `complete`, and its container's load event fires,
            // only after the module's load-delaying work is done. Speedometer's
            // Web Components suites intentionally initialize their shadow tree
            // from these modules and begin measuring from the iframe load event.
            let html = r##"<!doctype html><html><body>
                <output id="phase">waiting</output>
                <script>
                    const phase = document.getElementById("phase");
                    globalThis.topDclCount = 0;
                    window.addEventListener("DOMContentLoaded", function () { topDclCount++; });
                    const frame = document.createElement("iframe");
                    frame.srcdoc = '<template id="probe-template">' +
                        '<span id="ready">ready</span></template>' +
                        '<frame-load-probe></frame-load-probe><scr' +
                        'ipt type="module">' +
                        'customElements.define("frame-load-probe", class extends HTMLElement {' +
                        'connectedCallback() { const node = document.importNode(' +
                        'document.getElementById("probe-template").content, true);' +
                        'this.attachShadow({mode:"open"}).append(node); }});' +
                        'document.addEventListener("DOMContentLoaded", function () {' +
                        'document.body.setAttribute("data-dcl", document.readyState); });' +
                        'window.addEventListener("DOMContentLoaded", function () {' +
                        'document.body.setAttribute("data-window-dcl", document.readyState); });' +
                        '</scr' + 'ipt>';
                    frame.addEventListener("load", function () {
                        const child = frame.contentDocument;
                        const probe = child.querySelector("frame-load-probe");
                        phase.textContent = [
                            probe && probe.shadowRoot &&
                                probe.shadowRoot.querySelector("#ready").textContent,
                            child.body.getAttribute("data-dcl"),
                            child.body.getAttribute("data-window-dcl"),
                            child.readyState,
                            topDclCount
                        ].join("|");
                    });
                    document.body.appendChild(frame);
                </script>
            </body></html>"##;
            let (_handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));

            let mut last_html = String::new();
            let mut last_errors = Vec::new();
            let loaded = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, outcome }) => {
                            last_errors = outcome.errors;
                            if html.contains("ready|interactive|interactive|complete|1") {
                                assert!(last_errors.is_empty(), "{last_errors:?}");
                                break;
                            }
                            last_html = html;
                        }
                        Some(PageEvt::Trouble(errors)) => {
                            panic!("iframe module lifecycle failed: {errors:?}")
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before iframe load"),
                    }
                }
            })
            .await;
            assert!(
                loaded.is_ok(),
                "iframe load did not follow its parser module; errors={last_errors:?}, html={last_html}"
            );
        }

        #[tokio::test]
        async fn recreated_iframe_document_has_an_independent_module_map() {
            // HTML §8.1.3.2 assigns a module map to each environment settings
            // object, and §4.8.5 destroys an iframe's child navigable when the
            // element is removed. Reusing the same external module URL in a
            // new iframe must evaluate it again for that new Window/Document.
            let html = r##"<!doctype html><html><body>
                <output id="result">waiting</output>
                <script>
                    const results = [];
                    const moduleURL = "data:text/javascript,customElements.define(%22x-settings-repeat%22%2Cclass%20extends%20HTMLElement%7BconnectedCallback()%7Bthis.setAttribute(%22data-upgraded%22%2C%22yes%22)%7D%7D)%3B";
                    function installFrame() {
                        const frame = document.createElement("iframe");
                        frame.srcdoc = '<x-settings-repeat></x-settings-repeat><scr' +
                            'ipt>const recreatedFrameLexical = "yes";' +
                            'document.body.setAttribute("data-classic", recreatedFrameLexical);</scr' +
                            'ipt><scr' +
                            'ipt type="module" src="' + moduleURL + '"></scr' + 'ipt>';
                        frame.addEventListener("load", function () {
                            const element = frame.contentDocument.querySelector("x-settings-repeat");
                            results.push([
                                frame.contentDocument.body.getAttribute("data-classic"),
                                element && element.getAttribute("data-upgraded")
                            ].join(":"));
                            frame.remove();
                            if (results.length < 2) installFrame();
                            else document.getElementById("result").textContent = results.join("|");
                        });
                        document.body.insertBefore(frame, document.body.firstChild);
                    }
                    installFrame();
                </script>
            </body></html>"##;
            let (_handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));

            let mut last_html = String::new();
            let mut last_errors = Vec::new();
            let completed = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, outcome }) => {
                            last_errors = outcome.errors;
                            if html.contains("<output id=\"result\">yes:yes|yes:yes</output>") {
                                assert!(last_errors.is_empty(), "{last_errors:?}");
                                break;
                            }
                            last_html = html;
                        }
                        Some(PageEvt::Trouble(errors)) => {
                            panic!("recreated iframe module-map test failed: {errors:?}")
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before the second iframe module"),
                    }
                }
            })
            .await;
            assert!(
                completed.is_ok(),
                "second iframe reused the removed iframe's module map; errors={last_errors:?}, html={last_html}"
            );
        }

        #[tokio::test]
        async fn iframe_parser_module_is_started_once() {
            // HTML §4.12.1.1 gives every script element an `already
            // started` flag: preparation returns immediately once it is set.
            // A parser-created module therefore evaluates and fires `load`
            // exactly once. Starting an ordered module twice also advances the
            // parser's completion accounting twice and can expose iframe load
            // before a later application module has finished.
            let html = r##"<!doctype html><html><body>
                <output id="result">waiting</output>
                <script>
                    const frame = document.createElement("iframe");
                    frame.srcdoc =
                        '<scr' + 'ipt type="module">' +
                        'document.body.setAttribute("data-one", "ready");' +
                        '</scr' + 'ipt>' +
                        '<scr' + 'ipt type="module">' +
                        'document.body.setAttribute("data-two", "ready");' +
                        '</scr' + 'ipt>' +
                        '<scr' + 'ipt type="module">' +
                        'await new Promise(resolve => setTimeout(resolve, 20));' +
                        'document.body.setAttribute("data-three", "ready");' +
                        '</scr' + 'ipt>';
                    frame.addEventListener("load", function () {
                        const body = frame.contentDocument.body;
                        document.getElementById("result").textContent = [
                            body.getAttribute("data-one"),
                            body.getAttribute("data-two"),
                            body.getAttribute("data-three")
                        ].join("|");
                    });
                    document.body.appendChild(frame);
                </script>
            </body></html>"##;
            let (_handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));

            let mut last_html = String::new();
            let completed = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, outcome }) => {
                            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                            if html.contains("<output id=\"result\">ready|ready|ready</output>") {
                                break;
                            }
                            assert!(
                                !html.contains("<output id=\"result\">ready|"),
                                "iframe load preceded its final ordered module: {html}"
                            );
                            last_html = html;
                        }
                        Some(PageEvt::Trouble(errors)) => {
                            panic!("iframe module once-only check failed: {errors:?}")
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before iframe module settled"),
                    }
                }
            })
            .await;
            assert!(
                completed.is_ok(),
                "parser module ran or loaded more than once: {last_html}"
            );
        }
    }
}

#[cfg(feature = "lumen-backend")]
pub(crate) use desktop::spawn_page;
#[cfg(feature = "lumen-backend")]
pub(crate) use desktop::transform;

/// Lumen's implemented subset of the canonical TRust host boundary. Keep this declarative: tests
/// compare every entry with `js::HOST_FUNCTIONS`, while the table itself remains the single source
/// used to install functions into each new realm.
const LUMEN_HOST_FUNCTIONS: &[(&str, usize, NativeFn)] = &[
    ("__dom_create_element", 1, host_create_element),
    ("__dom_create_element_ns", 3, host_create_element_ns),
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
    ("__dom_assigned_slot", 1, host_assigned_slot),
    ("__dom_next", 1, host_next),
    ("__dom_prev", 1, host_prev),
    ("__dom_node_type", 1, host_node_type),
    ("__dom_tag", 1, host_tag),
    ("__dom_namespace", 1, host_namespace),
    ("__dom_element_name", 1, host_element_name),
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
    ("__dom_wrapper_subtree", 1, host_wrapper_subtree),
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
    ("__dom_run_classic_script", 3, host_run_classic_script),
    ("__dom_allocate_job_context", 0, host_allocate_job_context),
    ("__dom_create_window_realm", 8, host_create_window_realm),
    ("__dom_set_job_context", 1, host_set_job_context),
    ("__dom_release_job_context", 1, host_release_job_context),
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
    ("__dom_elements_from_point", 5, host_elements_from_point),
    ("__dom_scroll_get", 2, host_scroll_get),
    ("__dom_scroll_set", 3, host_scroll_set),
    ("__dom_load_frame", 3, host_load_frame),
    ("__cookie_get", 0, host_cookie_get),
    ("__cookie_set", 1, host_cookie_set),
    ("__clock_now", 0, host_clock_now),
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
    engine.set_host_job_context_hooks(host_enter_job_context, host_leave_job_context);
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
    {
        return None;
    }
    // Fetch Standard §5.6 invokes Fetch for every successfully constructed request. A count of
    // earlier page requests is neither a network error nor a specified rejection condition.
    network
        .fetched
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    // Fetch Body is a byte sequence. An ordinary ArrayBuffer crosses the host boundary without
    // depending on the realm's typed-array view bookkeeping; the prelude turns it into a view at
    // the point where the Fetch Body algorithms need one. Retain the legacy one-code-point-per-
    // byte string only as an allocation-failure fallback.
    let bytes = ctx
        .make_array_buffer(&body)
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
    let context = ctx.host_job_context();

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
                network.pending_fetches.insert(
                    id,
                    LumenPendingFetch {
                        context,
                        resolve: resolve.clone(),
                    },
                );
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

fn host_allocate_job_context(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    const MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
    let Some(state) = ctx.host_mut::<HostState>() else {
        return Err(ctx.make_error("InvalidStateError", "missing browser host state"));
    };
    let context = state.next_window_context;
    if context > MAX_SAFE_INTEGER {
        return Err(ctx.make_error(
            "QuotaExceededError",
            "exhausted HTML environment-settings identifiers",
        ));
    }
    state.next_window_context = context.saturating_add(1);
    Ok(Value::Num(context as f64))
}

fn platform_prelude_snapshot() -> Result<&'static [u8], String> {
    static SNAPSHOT: std::sync::OnceLock<Result<Vec<u8>, String>> = std::sync::OnceLock::new();
    match SNAPSHOT.get_or_init(|| lumen::compile_snapshot(crate::js::PRELUDE)) {
        Ok(snapshot) => Ok(snapshot.as_slice()),
        Err(error) => Err(error.clone()),
    }
}

/// HTML §7.2.2/§7.5.1 creates a fresh Window Realm for a new Document while
/// retaining the browsing context's WindowProxy identity. The native DOM arena
/// remains shared, but author globals, intrinsics, platform prototypes, module
/// maps, and promise-job settings belong to this newly-created Realm.
fn host_create_window_realm(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let context = args
        .first()
        .and_then(Value::as_num_opt)
        .filter(|value| value.is_finite() && *value > 0.0)
        .map_or(0, |value| value as u64);
    if context == 0 {
        return Err(ctx.make_error(
            "InvalidStateError",
            "a Window Realm requires a live environment-settings object",
        ));
    }
    let frame_id = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        let Some(frame_id) = host_arg_node(&dom, args, 1) else {
            return Err(ctx.make_error("InvalidStateError", "invalid navigable container"));
        };
        if !matches!(dom.tag_name(frame_id), Some("iframe" | "frame")) {
            return Err(ctx.make_error("InvalidStateError", "invalid navigable container"));
        }
        frame_id
    };

    let url = host_arg_string(ctx, args, 2);
    let source_config = args.get(3).cloned().unwrap_or(Value::Undefined);
    let parent_window = args.get(4).cloned().unwrap_or(Value::Undefined);
    let top_window = args.get(5).cloned().unwrap_or(Value::Undefined);
    let frame_element = args.get(6).cloned().unwrap_or(Value::Undefined);
    let agent_time_offset = args
        .get(7)
        .and_then(Value::as_num_opt)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(0.0);
    let snapshot = platform_prelude_snapshot().map_err(|error| {
        ctx.make_error(
            "SyntaxError",
            format!("compile TRust platform prelude: {error}"),
        )
    })?;
    // Realm creation and bootstrap form one synchronous publication operation.
    // Defer collection across both phases so partially initialized intrinsics
    // cannot be swept before the new global is published.
    ctx.suspend_gc();
    let realm = ctx.create_embed_realm();
    // Root the new Window Realm before running its platform bootstrap. The
    // bootstrap allocates a complete set of intrinsics and may cross an
    // allocation/GC safepoint; the host owns the nascent environment from the
    // moment HTML creates its navigable, even before the child global is
    // published through the iframe element.
    if let Some(state) = ctx.host_mut::<HostState>() {
        state.window_realms.insert(context, realm.clone());
    }
    let installed = ctx.with_embed_realm(&realm, |realm_ctx| {
        realm_ctx.set_host_job_context(context);
        for &(name, len, host_fn) in LUMEN_HOST_FUNCTIONS {
            realm_ctx.define_embed_global(name, len, host_fn);
        }

        let config = realm_ctx.new_object_with_proto(&Value::Null);
        for name in [
            "ua",
            "language",
            "languages",
            "width",
            "height",
            "devicePixelRatio",
            "hardwareConcurrency",
            "globalPrivacyControl",
            "secureContext",
        ] {
            if let Ok(value) = realm_ctx.member_get(&source_config, name) {
                realm_ctx.member_set(&config, name, value)?;
            }
        }
        realm_ctx.member_set(&config, "url", Value::from_string(url.clone()))?;
        realm_ctx.member_set(&config, "frameId", Value::Num(frame_id as f64))?;
        realm_ctx.member_set(&config, "hostSettingsContext", Value::Num(context as f64))?;
        realm_ctx.member_set(&config, "frameElement", frame_element.clone())?;
        realm_ctx.member_set(&config, "parentWindow", parent_window.clone())?;
        realm_ctx.member_set(&config, "topWindow", top_window.clone())?;
        realm_ctx.member_set(&config, "agentTimeOffset", Value::Num(agent_time_offset))?;
        let global = realm_ctx.global_this();
        realm_ctx.member_set(&global, "__trust_cfg", config)?;

        let bootstrap_result = realm_ctx.eval_classic_snapshot_interruptible(snapshot);
        match bootstrap_result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(EvalError::Throw(error))) => Err(error),
            Ok(Err(EvalError::Interrupted(reason))) => Err(realm_ctx.make_error(
                "AbortError",
                format!("Window Realm bootstrap interrupted: {}", reason.message()),
            )),
            Err(error) => Err(realm_ctx.make_error(
                "SyntaxError",
                format!(
                    "Window Realm bootstrap parse error at line {}: {}",
                    error.line, error.message
                ),
            )),
        }
    });
    ctx.resume_gc();
    match installed {
        Ok(Ok(())) => {}
        Ok(Err(error)) | Err(error) => {
            if let Some(state) = ctx.host_mut::<HostState>() {
                state.window_realms.remove(&context);
            }
            return Err(error);
        }
    }
    Ok(realm)
}

fn host_set_job_context(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let context = args
        .first()
        .and_then(Value::as_num_opt)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map_or(0, |value| value as u64);
    ctx.set_host_job_context(context);
    Ok(Value::Undefined)
}

fn host_release_job_context(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let context = args
        .first()
        .and_then(Value::as_num_opt)
        .filter(|value| value.is_finite() && *value > 0.0)
        .map_or(0, |value| value as u64);
    if context == 0 {
        return Ok(Value::Bool(false));
    }
    if let Some(state) = ctx.host_mut::<HostState>() {
        state.window_realms.remove(&context);
        if let Some(network) = state.network.as_mut() {
            network
                .pending_fetches
                .retain(|_, pending| pending.context != context);
        }
    }
    Ok(Value::Bool(ctx.release_host_job_context(context)))
}

fn host_enter_job_context(ctx: &mut Ctx, context: u64) {
    let _ = host_call_trust(ctx, "bindWindowSettings", &[Value::Num(context as f64)]);
}

fn host_leave_job_context(ctx: &mut Ctx) {
    let _ = host_call_trust(ctx, "restoreFrame", &[]);
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
    let context = ctx.host_job_context();
    let Some((events, pending)) = ctx.host_mut::<HostState>().and_then(|state| {
        let events = state.task_events.clone()?;
        state.pending_resources += 1;
        Some((events, state.pending_resources))
    }) else {
        return false;
    };
    if events
        .send(LumenHostTask::ResourceDone {
            context,
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
    let context = ctx.host_job_context();
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
            context,
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

fn host_eval_inline_classic(ctx: &mut Ctx, node_id: usize, name: &str, source: String) {
    let _ = host_call_trust(ctx, "bindFrameForNode", &[Value::Num(node_id as f64)]);
    let trust = host_trust(ctx).ok();
    let old_current = trust
        .as_ref()
        .and_then(|trust| ctx.member_get(trust, "currentScript").ok());
    if let Some(trust) = trust.as_ref() {
        let _ = ctx.member_set(trust, "currentScript", Value::Num(node_id as f64));
    }
    match ctx.eval_classic_script_interruptible(&source) {
        Ok(Ok(_)) => {}
        Ok(Err(EvalError::Throw(error))) => {
            let message = ctx
                .coerce_string(&error)
                .map(|message| message.to_string())
                .unwrap_or_else(|_| String::from("classic script threw"));
            host_push_injected_error(ctx, format!("{name}: {message}"));
        }
        Ok(Err(EvalError::Interrupted(reason))) => {
            host_push_injected_error(ctx, format!("{name}: {}", reason.message()));
        }
        Err(error) => host_push_injected_error(
            ctx,
            format!(
                "{name} parse error at line {}: {}",
                error.line, error.message
            ),
        ),
    }
    if let (Some(trust), Some(old_current)) = (trust.as_ref(), old_current) {
        let _ = ctx.member_set(trust, "currentScript", old_current);
    }
    let _ = host_call_trust(ctx, "restoreFrame", &[]);
}

/// HTML §8.1.4.4 "run a classic script": nested-document parser scripts are Script Records,
/// not indirect eval code. This synchronous entry is safe from a native platform callback and
/// retains the Realm's persistent global lexical environment across sibling script elements.
fn host_run_classic_script(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let node_id = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        let Some(node_id) = host_arg_node(&dom, args, 0) else {
            return Ok(Value::Undefined);
        };
        node_id
    };
    let source = host_arg_string(ctx, args, 1);
    let name = host_arg_string(ctx, args, 2);
    host_eval_inline_classic(ctx, node_id, &name, source);
    Ok(Value::Undefined)
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
        // HTML §4.12.1.1 creates a new JavaScript module script for each
        // inline script element. Its document URL is the base URL used to
        // resolve imports, not a shared module-map identity. Give Lumen a
        // stable per-element fragment so sibling inline modules cannot alias
        // one another while relative imports still resolve against `base`.
        let name = url::Url::parse(&base)
            .map(|mut url| {
                url.set_fragment(Some(&format!("inline-module-{node_id}")));
                url.to_string()
            })
            .unwrap_or_else(|_| format!("{base}#inline-module-{node_id}"));
        if !send_resource_completion(
            ctx,
            node_id,
            name,
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
        let name = state_base(ctx);
        host_eval_inline_classic(ctx, node_id, &name, text);
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
        let queued_command = match ctl_rx.try_recv() {
            Ok(command) => Some(command),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        };
        // This is a forced full host collection, so use it only as the idle
        // hook it is: after proving no message is runnable and immediately
        // before an indefinite park. A worker with a future timer remains
        // active and uses Lumen's allocation/task-boundary collection instead
        // of tracing its entire heap after every message or timer task.
        if queued_command.is_none() && deadline.is_none() {
            engine.collect_garbage_at_idle();
        }
        let command = match queued_command {
            Some(command) => Some(command),
            None => match deadline {
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
        // HTML's fetch-a-module-script graph algorithm requires this dependency. Keep the count
        // as diagnostics, but never turn historical activity into a synthetic load failure.
        fetched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        // ECMA-262 HostLoadImportedModule plus HTML's module-script fetch do not permit a host to
        // fail a required dynamic import merely because this Document made earlier requests.
        network
            .fetched
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                    |count| {
                        (count < crate::js::MAX_PAGE_FETCHES_WITH_SPECULATION).then_some(count + 1)
                    },
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
    // HTML runs a module with its preparation-time Document's realm/settings.
    // TRust multiplexes same-agent nested Window realms through one Lumen
    // global, so enter the script element's owning frame for evaluation and
    // the immediately-following microtask checkpoint. This is the module
    // counterpart of `run_injected_classic_task` above.
    let _ = host_call_trust(
        engine.ctx(),
        "bindFrameForNode",
        &[Value::Num(node_id as f64)],
    );
    let result = (|| {
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
    })();
    let _ = host_call_trust(engine.ctx(), "restoreFrame", &[]);
    result
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
            let pending = engine
                .ctx()
                .host_mut::<HostState>()
                .and_then(|state| state.network.as_mut())
                .and_then(|network| network.pending_fetches.remove(&id));
            let Some(pending) = pending else {
                return Ok(());
            };
            // Fetch §2 records a global object as the fetch params' task
            // destination and queues response processing on that global's
            // networking task source. Construct the Response payload in that
            // Realm as well: Fetch §5.3 consumes the Body using the response
            // object's relevant global, including its ArrayBuffer intrinsics.
            let context = pending.context;
            let resolve = pending.resolve;
            let run = move |engine: &mut lumen::Engine| {
                let value = host_fetch_result_value(engine.ctx(), result);
                engine
                    .call_function_interruptible(&resolve, Value::Undefined, &[value])
                    .map_err(|error| describe_eval_error(engine, error, "fetch networking task"))
            };
            if context == engine.ctx().host_job_context() {
                run(engine)?;
            } else {
                let realm = engine
                    .ctx()
                    .host_mut::<HostState>()
                    .and_then(|state| state.window_realms.get(&context).cloned());
                let Some(realm) = realm else {
                    // A task associated with a no-longer-active Document is
                    // not runnable on the Window event loop.
                    return Ok(());
                };
                match engine.with_embed_realm(&realm, run) {
                    Ok(result) => {
                        result?;
                    }
                    Err(error) => {
                        let message = engine
                            .ctx()
                            .coerce_string(&error)
                            .map(|message| message.to_string())
                            .unwrap_or_else(|_| String::from("unknown Window Realm"));
                        return Err(format!("fetch task Realm: {message}"));
                    }
                }
            }
        }
        LumenHostTask::ResourceDone {
            context,
            node_id,
            name,
            kind,
            result,
            external,
        } => {
            if let Some(state) = engine.ctx().host_mut::<HostState>() {
                state.pending_resources = state.pending_resources.saturating_sub(1);
            }
            // HTML §8.1.7.2 queues element work against the element's relevant
            // global/Document. A parallel resource fetch therefore retains
            // the environment-settings token captured at preparation time;
            // dispatching through the actor's root Realm would fire load/error
            // against the wrong Realm-local listener registry.
            if context == engine.ctx().host_job_context() {
                run_resource_task(engine, node_id, name, kind, result, external)?;
            } else {
                let realm = engine
                    .ctx()
                    .host_mut::<HostState>()
                    .and_then(|state| state.window_realms.get(&context).cloned());
                let Some(realm) = realm else {
                    // Its Document was replaced while the fetch was in
                    // flight. Tasks whose document is no longer active are
                    // not runnable in the Window event loop.
                    return Ok(());
                };
                match engine.with_embed_realm(&realm, move |engine| {
                    run_resource_task(engine, node_id, name, kind, result, external)
                }) {
                    Ok(result) => result?,
                    Err(error) => {
                        let message = engine
                            .ctx()
                            .coerce_string(&error)
                            .map(|message| message.to_string())
                            .unwrap_or_else(|_| String::from("unknown Window Realm"));
                        return Err(format!("resource task Realm: {message}"));
                    }
                }
            }
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

fn host_create_element_ns(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let namespace = host_arg_string(ctx, args, 0);
    let prefix = host_arg_string(ctx, args, 1);
    let local_name = host_arg_string(ctx, args, 2);
    let dom = host_dom(ctx);
    let id = dom.borrow_mut().create_element_ns(
        &namespace,
        (!prefix.is_empty()).then_some(prefix.as_str()),
        &local_name,
    );
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

/// DOM Standard §4.2.2.3 assigned-slot lookup. Unlike the public
/// `assignedSlot` getter, this internal primitive intentionally returns slots
/// in closed roots: Node's event-parent algorithm must traverse them.
fn host_assigned_slot(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(host_id_value(
        host_arg_node(&dom, args, 0).and_then(|id| dom.assigned_slot(id)),
    ))
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

fn host_clock_now(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(
        ctx.host_mut::<HostState>()
            .map(|state| state.clock.now_ms())
            .unwrap_or(0.0),
    ))
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

fn host_element_name(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let Some(id) = host_arg_node(&dom, args, 0) else {
        return Ok(Value::Null);
    };
    let Some(local_name) = dom.tag_name(id) else {
        return Ok(Value::Null);
    };
    Ok(ctx.make_array(vec![
        Value::from_string(local_name.to_owned()),
        dom.namespace_uri(id)
            .map(|namespace| Value::from_string(namespace.to_owned()))
            .unwrap_or(Value::Null),
        dom.namespace_prefix(id)
            .map(|prefix| Value::from_string(prefix.to_owned()))
            .unwrap_or(Value::Null),
    ]))
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

fn host_wrapper_subtree(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let ids = {
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
            .map(|root| dom.wrapper_subtree_ids(root))
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

/// Keep the geometry used by CSSOM View reads on the same epoch-keyed layout pass as the other
/// JavaScript adapter. The resulting rectangles remain floating-point CSS pixels; terminal
/// quantization is still confined to `layout2::paint`.
fn ensure_host_geom_cache(ctx: &mut Ctx, reason: &'static str) -> Rc<RefCell<LumenGeomCache>> {
    let (dom_handle, base, viewport, cache, images, hit_testing_active) = {
        let state = ctx
            .host_mut::<HostState>()
            .expect("HostState installed before any Lumen host call");
        (
            state.dom.clone(),
            state.base.clone(),
            state.viewport.get(),
            state.geom_cache.clone(),
            state.images.clone(),
            state.hit_testing_active.get(),
        )
    };
    let dom = dom_handle.borrow();
    let epoch = dom.epoch();
    let mut cached = cache.borrow_mut();
    let rebuilt = cached.epoch != epoch;
    if rebuilt {
        let measure_started = std::env::var_os("TRUST_DIAG_FRAME")
            .is_some()
            .then(Instant::now);
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let (boxes, tracks, scrolling_areas, paint) = if hit_testing_active {
            let (boxes, tracks, scrolling_areas, paint) =
                crate::layout2::measure_cssom_with_paint_css(
                    &dom,
                    &base,
                    viewport,
                    &forms,
                    &controls,
                    &images.borrow(),
                );
            (boxes, tracks, scrolling_areas, Some(paint))
        } else {
            let (boxes, tracks, scrolling_areas) = crate::layout2::measure_boxes_css(
                &dom,
                &base,
                viewport,
                &forms,
                &controls,
                &images.borrow(),
            );
            (boxes, tracks, scrolling_areas, None)
        };
        if let Some(measure_started) = measure_started {
            let cascade = crate::dom::take_casc_diag();
            eprintln!(
                "DIAGGEOM reason={reason} nodes={} total={}ms cascade={}ms matched={}builds/{}candidates/{}ms css_parse={}ms rules={}",
                dom.node_count(),
                measure_started.elapsed().as_millis(),
                cascade.cascaded_us / 1000,
                cascade.matched_rule_builds,
                cascade.matched_candidates,
                cascade.matched_us / 1000,
                cascade.style_index_us / 1000,
                cascade.rules,
            );
        }
        cached.boxes = boxes;
        cached.tracks = tracks;
        cached.scrolling_areas = scrolling_areas;
        cached.paint = paint;
        cached.epoch = epoch;
        cached.top_document_valid = true;
    }
    drop(cached);
    drop(dom);
    if rebuilt {
        // The full measure incorporated every mutation through `epoch`; begin the next
        // document-scope classification window from an empty invalidation log.
        let _ = dom_handle.borrow_mut().take_geometry_dirty_targets();
    }
    cache
}

/// CSSOM View §5 `elementsFromPoint()`: return paint-ordered arena node ids.
/// The JavaScript binding performs WebIDL conversion, viewport bounds checks,
/// wrapper conversion, and the required root-element fallback.
fn host_elements_from_point(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let scope = args
        .first()
        .and_then(Value::as_num_opt)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as usize)
        .unwrap_or(DOCUMENT);
    let x = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as f32;
    let y = args.get(2).and_then(Value::as_num_opt).unwrap_or(0.0) as f32;
    let scroll_x = args.get(3).and_then(Value::as_num_opt).unwrap_or(0.0) as f32;
    let scroll_y = args.get(4).and_then(Value::as_num_opt).unwrap_or(0.0) as f32;

    let viewport = {
        let state = ctx
            .host_mut::<HostState>()
            .expect("HostState installed before any Lumen host call");
        if !state.hit_testing_active.replace(true) || state.geom_cache.borrow().paint.is_none() {
            state.geom_cache.borrow_mut().epoch = u64::MAX;
        }
        state.viewport.get()
    };
    let cache = ensure_host_geom_cache(ctx, "elements-from-point");
    let cached = cache.borrow();
    let Some(paint) = cached.paint.as_ref() else {
        return Ok(ctx.make_array(Vec::new()));
    };
    let point = if scope == DOCUMENT {
        crate::core::CssPoint::new(x, y)
    } else if let Some(frame) = paint
        .scroll_containers
        .iter()
        .find(|container| container.node == scope)
    {
        crate::core::CssPoint::new(
            frame.viewport.x - scroll_x + x,
            frame.viewport.y - scroll_y + y,
        )
    } else {
        return Ok(ctx.make_array(Vec::new()));
    };
    let hits = crate::render::page_element_hits_at(
        paint,
        crate::core::CssSize::new(viewport.width, viewport.height),
        crate::core::CssPoint::new(scroll_x, scroll_y),
        point,
    );
    drop(cached);

    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let wanted_frame = (scope != DOCUMENT).then_some(scope);
    let nodes = hits
        .into_iter()
        .filter(|hit| dom.frame_owner(hit.node) == wanted_frame)
        .map(|hit| Value::Num(hit.node as f64))
        .collect();
    Ok(ctx.make_array(nodes))
}

fn host_resolved_grid_tracks(ctx: &mut Ctx, args: &[Value], columns: bool) -> Option<String> {
    let cache = ensure_host_geom_cache(ctx, "computed-grid-tracks");
    let id = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)?
    };
    let cached = cache.borrow();
    let (column_tracks, row_tracks) = cached.tracks.get(&id)?;
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

fn host_resolved_box_size(ctx: &mut Ctx, args: &[Value], width: bool) -> Option<String> {
    let cache = ensure_host_geom_cache(ctx, "computed-box-size");
    let id = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)?
    };
    let cached = cache.borrow();
    let rect = cached.boxes.get(&id)?;
    let value = if width {
        rect.css_width?
    } else {
        rect.css_height?
    };
    Some(crate::js_host_boundary::serialize_css_px(value))
}

/// CSSOM §7.2/§9 resolved-value backing. Grid track lists are used values captured by the same
/// layout pass; all other properties come from the canonical DOM cascade.
fn host_computed_style(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let name = host_arg_string(ctx, args, 1);
    if (name == "width" || name == "height")
        && let Some(value) = host_resolved_box_size(ctx, args, name == "width")
    {
        return Ok(Value::from_string(value));
    }
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
/// availability state is injected into this backend, synchronously available data URLs are the
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
    let dom_handle = host_dom(ctx);
    let cache = ctx
        .host_mut::<HostState>()
        .expect("HostState installed before any Lumen host call")
        .geom_cache
        .clone();
    let (id, epoch, top_level_frame) = {
        let dom = dom_handle.borrow();
        let id = host_arg_node(&dom, args, 0);
        let top_level_frame = id.is_some_and(|id| {
            matches!(dom.tag_name(id), Some("iframe" | "frame")) && dom.frame_owner(id).is_none()
        });
        (id, dom.epoch(), top_level_frame)
    };
    let (cached_epoch, top_document_valid) = {
        let cached = cache.borrow();
        (cached.epoch, cached.top_document_valid)
    };
    let reuse_cached = if cached_epoch == epoch {
        true
    } else if cached_epoch != u64::MAX && top_document_valid && top_level_frame {
        // HTML §7.3.1.3 makes an iframe's content navigable own a distinct active Document.
        // TRust keeps those nodes in the page arena, so its broad epoch advances for both
        // Documents. CSSOM View asks for the embedding element's border box in the container
        // Document; changes confined to nested Documents cannot alter that box. Consume the
        // independent geometry log to prove that confinement. Any unattributed or container-
        // Document mutation permanently rejects reuse until a full measure refreshes the cache.
        let changes = dom_handle.borrow_mut().take_geometry_dirty_targets();
        let nested_documents_only = changes.is_some_and(|nodes| {
            let dom = dom_handle.borrow();
            let mut rejected = Vec::new();
            for (node, kind) in nodes {
                let owner = dom.frame_owner(node);
                let tag = dom.tag_name(node);
                // CSSOM View getClientRects returns no rectangles without an associated box.
                // A currently disconnected mutation therefore cannot affect a connected frame;
                // if that subtree is subsequently inserted, the insertion records its connected
                // parent as a separate invalidation before any synchronous geometry read.
                let nested = !dom.is_connected(node)
                    || owner.is_some()
                    || (kind == crate::dom::DirtyKind::Content
                        && matches!(tag, Some("iframe" | "frame")));
                if !nested && rejected.len() < 8 {
                    rejected.push((node, kind, tag.map(str::to_string), owner));
                }
            }
            if !rejected.is_empty() && std::env::var_os("TRUST_DIAG_FRAME").is_some() {
                eprintln!("DIAGGEOM top-document-invalidations={rejected:?}");
            }
            rejected.is_empty()
        });
        if !nested_documents_only {
            cache.borrow_mut().top_document_valid = false;
        }
        nested_documents_only
    } else {
        false
    };
    let cache = if reuse_cached {
        cache
    } else {
        ensure_host_geom_cache(ctx, "bounding-rect")
    };
    let rect = id.and_then(|id| cache.borrow().boxes.get(&id).copied());
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
        let cache = ensure_host_geom_cache(ctx, "scrolling-area");
        let id = {
            let dom = host_dom(ctx);
            let dom = dom.borrow();
            host_arg_node(&dom, args, 0)
        };
        id.and_then(|id| cache.borrow().scrolling_areas.get(&id).copied())
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
        assert_eq!(canonical.len(), 112, "canonical host boundary changed");
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
        assert_eq!(LUMEN_HOST_FUNCTIONS.len(), 112);

        let mut engine = platform_engine();
        for &(name, length, _) in LUMEN_HOST_FUNCTIONS {
            let actual = eval_value(&mut engine, &format!("{name}.length"), name).unwrap();
            assert_eq!(actual.as_num_opt(), Some(length as f64), "{name}.length");
        }
    }

    #[test]
    fn create_element_ns_preserves_expanded_names_and_element_interfaces() {
        // WHATWG DOM §1.4 "validate and extract" and §4.5 createElementNS:
        // namespace, prefix, and local name select the element interface and
        // remain observable. This is the construction path React uses for SVG;
        // losing the namespace made Stockcharts omit its interactive SVG layer.
        let mut engine = platform_engine();
        let actual = string_value(
            &mut engine,
            r#"(() => {
                const SVG = 'http://www.w3.org/2000/svg';
                const MATH = 'http://www.w3.org/1998/Math/MathML';
                const XML = 'http://www.w3.org/XML/1998/namespace';
                const XMLNS = 'http://www.w3.org/2000/xmlns/';
                const svg = document.createElementNS(SVG, 'svg');
                const rect = document.createElementNS(SVG, 'rect');
                rect.setAttribute('class', 'react-stockcharts-crosshair-cursor');
                svg.appendChild(rect);
                const host = document.createElement('div');
                host.appendChild(svg);
                const math = document.createElementNS(MATH, 'math');
                const plain = document.createElementNS(null, 'widget');
                const prefixed = document.createElementNS(XML, 'xml:item');
                host.insertAdjacentHTML('beforeend', '<svg><path id="parsed-svg"/></svg>');
                const parsed = host.querySelector('#parsed-svg');
                function errorName(run) {
                    try { run(); return 'none'; } catch (error) { return error.name; }
                }
                return [
                    svg.namespaceURI === SVG,
                    svg.localName === 'svg', svg.tagName === 'svg', svg.prefix === null,
                    svg instanceof SVGElement, svg instanceof SVGSVGElement,
                    !(svg instanceof HTMLElement),
                    rect instanceof SVGRectElement,
                    Object.prototype.toString.call(rect) === '[object SVGRectElement]',
                    host.querySelector('.react-stockcharts-crosshair-cursor') === rect,
                    math.namespaceURI === MATH, math instanceof MathMLElement,
                    plain.namespaceURI === null, plain.constructor === Element,
                    prefixed.prefix === 'xml', prefixed.localName === 'item',
                    prefixed.tagName === 'xml:item', prefixed.nodeName === 'xml:item',
                    parsed instanceof SVGPathElement,
                    errorName(() => document.createElementNS()),
                    errorName(() => document.createElementNS(null, '')),
                    errorName(() => document.createElementNS(null, 'p:item')),
                    errorName(() => document.createElementNS(SVG, 'xml:item')),
                    errorName(() => document.createElementNS(SVG, 'xmlns')),
                    errorName(() => document.createElementNS(XMLNS, 'item'))
                ].join(',');
            })()"#,
        );
        assert_eq!(
            actual,
            "true,true,true,true,true,true,true,true,true,true,true,true,true,true,true,true,true,true,true,TypeError,InvalidCharacterError,NamespaceError,NamespaceError,NamespaceError,NamespaceError"
        );
    }

    #[test]
    fn webassembly_boundary_preserves_store_identity_memory_and_reentry() {
        // WebAssembly JS Interface §§4.1–4.2 and 5.1–5.6: one store per agent, one wrapper per
        // address, imported host calls see the current memory Data Block, memory growth detaches
        // the previous fixed buffer, and i64 crosses the JS boundary as BigInt.
        let mut engine = platform_engine();
        let mut module = wat::parse_str(
            r##"
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
            "##,
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
    fn webassembly_legacy_tagged_exceptions_unwind_to_the_matching_catch() {
        // Legacy WebAssembly exception handling: a throw transfers control to the nearest
        // enclosing matching catch and preserves the tag payload on the Wasm operand stack.
        let mut engine = platform_engine();
        let module = wat::parse_str(
            r#"
            (module
              (tag $tag (param i32))
              (func (export "caught") (result i32)
                try (result i32)
                  i32.const 7
                  throw $tag
                catch $tag
                  drop
                  i32.const 42
                end)
              (func (export "uncaught")
                i32.const 9
                throw $tag))
            "#,
        )
        .unwrap();
        let bytes = engine
            .ctx()
            .make_uint8array(&module)
            .unwrap_or_else(|_| panic!("make exception fixture"));
        let global = engine.global_this();
        engine
            .ctx()
            .member_set(&global, "exceptionFixture", bytes)
            .unwrap_or_else(|_| panic!("install exception fixture"));
        eval(
            &mut engine,
            r#"
            const exceptionInstance = new WebAssembly.Instance(
                new WebAssembly.Module(exceptionFixture));
            let uncaught = false;
            try { exceptionInstance.exports.uncaught(); }
            catch (error) { uncaught = !!error; }
            globalThis.exceptionResult = [
                exceptionInstance.exports.caught(), uncaught
            ].join('|');
            "#,
            "WebAssembly legacy exceptions",
        )
        .unwrap();
        assert_eq!(string_value(&mut engine, "exceptionResult"), "42|true");
    }

    #[test]
    fn fetch_preparation_has_no_cumulative_request_cliff() {
        // Fetch Standard §5.6 creates a request and invokes Fetch for every valid call. Earlier
        // activity in the same Document is not a network error and cannot make a later call fail.
        // This deliberately crosses the former 256-request cutoff without performing network I/O.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let page = url::Url::parse(DEFAULT_URL).unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        let (task_tx, _task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page, runtime.handle().clone(), cache, task_tx);

        for index in 0..320 {
            let (_, _, request) = prepare_host_request(
                &mut state,
                &format!("/api/{index}"),
                String::from("GET"),
                None,
                Vec::new(),
            )
            .unwrap_or_else(|| panic!("valid request {index} was denied by historical activity"));
            assert_eq!(request.url.path(), format!("/api/{index}"));
        }
        assert_eq!(
            state
                .network
                .as_ref()
                .map(|network| network.fetched.load(std::sync::atomic::Ordering::Relaxed)),
            Some(320)
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
    fn iframe_fetch_reactions_run_with_their_relevant_window() {
        // ECMA-262 NewPromiseReactionJob associates a job with its handler's Realm. HTML
        // HostEnqueuePromiseJob then prepares that Realm's settings object before running the
        // job. An async continuation created by a child Window must consequently observe the
        // child Document after its fetch completes, even though the networking task is selected
        // while no child script is synchronously on the stack.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let page = url::Url::parse(DEFAULT_URL).unwrap();
        let response_url = page.join("api").unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        cache.seed(
            response_url.to_string(),
            200,
            String::from("text/plain"),
            b"ready".to_vec(),
        );
        let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page, runtime.handle().clone(), cache, task_tx);
        let mut engine = configured_engine(state, DEFAULT_URL);

        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                frame.srcdoc = '<body><script>' +
                    '(async function () {' +
                    'const response = await fetch("/api");' +
                    'const bytes = Array.from(new Uint8Array(await response.clone().arrayBuffer())).join(",");' +
                    'const text = await response.text();' +
                    'globalThis.fetchBodyResult = [response instanceof Response, text, bytes].join("|");' +
                    'const marker = document.createElement("div");' +
                    'marker.id = "child-ready";' +
                    'document.body.appendChild(marker);' +
                    '})()' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();
                globalThis.fetchFrame = frame;
            "##,
            "iframe fetch setup",
        )
        .unwrap();
        run_microtask_checkpoint(&mut engine);

        let task = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), task_rx.recv()).await })
            .expect("iframe fetch completes")
            .expect("network task channel remains open");
        dispatch_host_task(&mut engine, task).unwrap();
        run_microtask_checkpoint(&mut engine);

        assert_eq!(
            string_value(
                &mut engine,
                "(() => { const marker = fetchFrame.contentDocument.querySelector('#child-ready'); return String(marker.parentNode === fetchFrame.contentDocument.body) + '|' + String(marker.parentNode === document.body); })()",
            ),
            "true|false"
        );
        assert_eq!(
            string_value(&mut engine, "fetchFrame.contentWindow.fetchBodyResult"),
            "true|ready|114,101,97,100,121",
            "Fetch response objects and body bytes belong to the initiating Window Realm"
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
                    Array.from(new Uint8Array(syncResponse[3])).join(',')
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
    fn iframe_stylesheet_and_load_tasks_run_in_their_relevant_realms() {
        // HTML §8.1.7.2 queues asynchronous element work against the
        // element's relevant global, and the iframe load-event steps run only
        // after the child Window's load task. A stylesheet fetched for a
        // child Document must consequently settle the child listener registry
        // before the owner-Document iframe element fires load.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let page = url::Url::parse(DEFAULT_URL).unwrap();
        let stylesheet_url = page.join("frame.css").unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        cache.seed_with_headers(
            stylesheet_url.to_string(),
            200,
            String::from("text/css"),
            vec![(String::from("content-type"), String::from("text/css"))],
            b"#realm-style-target { display: grid; }".to_vec(),
        );
        let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page, runtime.handle().clone(), cache, task_tx);
        let mut engine = configured_engine(state, DEFAULT_URL);

        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                globalThis.frameLifecycleOrder = [];
                const frame = document.createElement("iframe");
                frame.onload = function () { frameLifecycleOrder.push("frame"); };
                frame.srcdoc = '<head><link id="child-style" rel="stylesheet" href="/frame.css"></head>' +
                    '<body><div id="realm-style-target"></div><script>' +
                    'document.getElementById("child-style").addEventListener("load", function () {' +
                    'parent.frameLifecycleOrder.push("style:" + getComputedStyle(' +
                    'document.getElementById("realm-style-target")).display);' +
                    '});' +
                    'window.addEventListener("load", function () {' +
                    'parent.frameLifecycleOrder.push("window");' +
                    '});' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();
                globalThis.stylesheetFrame = frame;
            "##,
            "iframe stylesheet Realm task setup",
        )
        .unwrap();
        run_microtask_checkpoint(&mut engine);

        let task = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), task_rx.recv()).await })
            .expect("iframe stylesheet completes")
            .expect("resource task channel remains open");
        dispatch_host_task(&mut engine, task).unwrap();
        run_microtask_checkpoint(&mut engine);

        for _ in 0..8 {
            let has_task = engine_call_trust_method(&mut engine, "hasPlatformTask", &[])
                .map(|value| engine.ctx().to_boolean(&value))
                .unwrap_or(false);
            if !has_task {
                break;
            }
            assert!(
                engine_call_trust_method(&mut engine, "runPlatformTask", &[])
                    .is_ok_and(|value| engine.ctx().to_boolean(&value))
            );
            run_microtask_checkpoint(&mut engine);
        }
        let errors = string_value(&mut engine, "__trust.takeErrors()");
        let logs = string_value(&mut engine, "__trust.takeLogs()");
        assert_eq!(
            string_value(&mut engine, "frameLifecycleOrder.join('|')"),
            "style:grid|window|frame",
            "errors={errors}; logs={logs}"
        );
        assert_eq!(
            string_value(&mut engine, "stylesheetFrame.contentDocument.readyState",),
            "complete"
        );
        assert_eq!(errors, "");
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
    fn slotted_events_follow_the_assigned_slot_through_shadow_buttons() {
        // DOM §§2.2, 2.9, 4.2.2.3, and 4.4: a slottable's event parent is
        // its assigned slot rather than its light-tree parent. This is what
        // lets a click targeting a component's projected label reach the
        // shadow button that contains <slot>. Closed roots remain in the
        // internal path but are hidden by assignedSlot/composedPath outside.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html);
            html.appendChild(body);
            const names = (path) => path.map((node) => node === window ? "window"
                : node.localName || (node.nodeType === 9 ? "#document" : "#fragment")).join(",");

            const openHost = document.createElement("x-open");
            body.appendChild(openHost);
            const openRoot = openHost.attachShadow({ mode: "open" });
            openRoot.innerHTML = '<button><slot name="label"></slot></button>';
            const openButton = openRoot.querySelector("button");
            const openSlot = openRoot.querySelector("slot");
            const openLabel = document.createElement("span");
            openLabel.setAttribute("slot", "label");
            openLabel.textContent = "Weekly views";
            openHost.appendChild(openLabel);
            let openReached = false, openTarget = "", openPath = "", documentTarget = "";
            openButton.addEventListener("click", (event) => {
                openReached = true;
                openTarget = event.target.localName;
                openPath = names(event.composedPath());
            });
            document.addEventListener("click", (event) => { documentTarget = event.target.localName; });
            openLabel.dispatchEvent(new MouseEvent("click", { bubbles: true, composed: true }));

            const closedHost = document.createElement("x-closed-slot");
            body.appendChild(closedHost);
            const closedRoot = closedHost.attachShadow({ mode: "closed" });
            closedRoot.innerHTML = '<button><slot name="label"></slot></button>';
            const closedButton = closedRoot.querySelector("button");
            const closedLabel = document.createElement("span");
            closedLabel.setAttribute("slot", "label");
            closedHost.appendChild(closedLabel);
            let closedReached = false, closedInsidePath = "", closedOutsidePath = "";
            closedButton.addEventListener("click", (event) => {
                closedReached = true;
                closedInsidePath = names(event.composedPath());
            });
            closedHost.addEventListener("click", (event) => {
                closedOutsidePath = names(event.composedPath());
            });
            closedLabel.dispatchEvent(new MouseEvent("click", { bubbles: true, composed: true }));

            const textHost = document.createElement("x-text-slot");
            body.appendChild(textHost);
            const textRoot = textHost.attachShadow({ mode: "open" });
            textRoot.innerHTML = "<slot></slot>";
            const projectedText = document.createTextNode("projected");
            textHost.appendChild(projectedText);

            globalThis.slottedEventResult = [
                openLabel.parentNode === openHost,
                openLabel.assignedSlot === openSlot,
                openReached,
                openTarget,
                documentTarget,
                openPath,
                closedLabel.assignedSlot === null,
                closedReached,
                closedInsidePath,
                closedOutsidePath,
                projectedText.assignedSlot === textRoot.querySelector("slot")
            ].join("|");
            "##,
            "assigned-slot event parent",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "slottedEventResult"),
            "true|true|true|span|span|span,slot,button,#fragment,x-open,body,html,#document,window|true|true|span,slot,button,#fragment,x-closed-slot,body,html,#document,window|span,x-closed-slot,body,html,#document,window|true"
        );
    }

    #[test]
    fn hyperlink_activation_uses_the_click_event_path() {
        // DOM §2.9 chooses the first activation-behavior object while building
        // the click event path. It can therefore be an ancestor of a Text or
        // Element target, can be reached through an assigned slot, and remains
        // the activation target even if a listener removes it before the
        // default action runs. HTML links with an empty href are hyperlinks too.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html);
            html.appendChild(body);

            const direct = document.createElement("a");
            direct.href = "/details/direct";
            const label = document.createElement("span");
            label.textContent = "direct";
            direct.appendChild(label);
            body.appendChild(direct);
            const text = label.firstChild;

            const host = document.createElement("x-link");
            const root = host.attachShadow({ mode: "closed" });
            root.innerHTML = '<a href="/details/slotted"><slot></slot></a>';
            const slotted = document.createElement("img");
            host.appendChild(slotted);
            body.appendChild(host);

            const removed = document.createElement("a");
            removed.href = "/details/removed";
            const removedLabel = document.createElement("span");
            removed.appendChild(removedLabel);
            body.appendChild(removed);
            removed.addEventListener("click", () => {
                removedLabel.remove();
                removed.remove();
            });
            const removedPrevented = __trust.click(removedLabel.__id);
            const removedDefault = __trust.followAnchorDefault(removedLabel.__id);

            const canceled = document.createElement("a");
            canceled.href = "/details/canceled";
            const canceledImage = document.createElement("img");
            canceled.appendChild(canceledImage);
            body.appendChild(canceled);
            canceled.addEventListener("click", event => event.preventDefault());

            const empty = document.createElement("a");
            empty.setAttribute("href", "");
            body.appendChild(empty);

            globalThis.hyperlinkActivationResult = [
                __trust.followAnchorDefault(text.__id),
                __trust.followAnchorDefault(slotted.__id),
                removedPrevented,
                removedDefault,
                __trust.click(canceledImage.__id),
                __trust.followAnchorDefault(empty.__id)
            ].join("|");
            "##,
            "hyperlink activation target",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "hyperlinkActivationResult"),
            "https://example.com/details/direct|https://example.com/details/slotted|false|https://example.com/details/removed|true|https://example.com/"
        );
    }

    #[test]
    fn detached_node_listeners_remain_observable_without_staying_render_roots() {
        // DOM removal does not erase a Node's event listener list: retained
        // detached nodes still dispatch, and reinsertion restores them to the
        // rendered event-target census. The host registry must hold detached
        // targets weakly so it does not become their only lifetime owner.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            const button = document.createElement("button");
            body.appendChild(button);
            let hits = 0;
            button.addEventListener("click", () => hits++);
            const id = button.__id;
            button.remove();
            button.click();
            const detachedIsRendered = __trust.clickables().indexOf(id) >= 0;
            body.appendChild(button);
            const reinsertedIsRendered = __trust.clickables().indexOf(id) >= 0;
            button.click();
            globalThis.detachedListenerResult = [
                hits, detachedIsRendered, reinsertedIsRendered
            ].join("|");
            "##,
            "detached event-listener retention",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "detachedListenerResult"),
            "2|false|true"
        );
    }

    #[test]
    fn css_style_declaration_rejects_unitless_nonzero_lengths() {
        // CSSOM §6.7.1 + CSS Values 4 §6: assigning a JS number to a length
        // property stringifies it, but the resulting nonzero <number> is not a
        // <length> and must leave the declaration block unchanged.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const head = document.createElement("head");
            const body = document.createElement("body");
            document.appendChild(html);
            html.appendChild(head);
            html.appendChild(body);
            const style = document.createElement("style");
            style.textContent = "#frame { height: 100%; }";
            head.appendChild(style);
            const frame = document.createElement("iframe");
            frame.id = "frame";
            body.appendChild(frame);
            frame.style.height = innerHeight;
            const invalidProperty = frame.style.height;
            frame.style.width = 0;
            globalThis.cssLengthAssignmentResult = [
                invalidProperty,
                getComputedStyle(frame).height,
                frame.style.width,
                CSS.supports("height", "518"),
                CSS.supports("height", "0")
            ].join("|");
            "##,
            "CSSStyleDeclaration length validation",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "cssLengthAssignmentResult"),
            "|100%|0|false|true"
        );
    }

    #[test]
    fn hyperlink_url_components_update_href_with_url_setter_semantics() {
        // HTML §4.6.3 (HyperlinkElementUtils): component setters parse with the
        // URL state override and then update the href content attribute. SCM
        // Music Player uses an <a> as a URL builder before creating its iframe;
        // a no-op pathname setter sends that iframe to script.js instead of the
        // player document.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const a = document.createElement("a");
            a.href = "/script.js";
            const absent = document.createElement("a");
            absent.pathname = "/should-not-create-a-url";
            a.pathname = "player";
            a.search = "hostBridge=1";
            a.hash = "destination";
            const area = document.createElement("area");
            area.href = "https://example.test/old";
            area.pathname = "/new";
            globalThis.hyperlinkSetterResult = [
                a.href,
                a.getAttribute("href"),
                a.pathname,
                a.search,
                a.hash,
                area.href,
                absent.hasAttribute("href"),
                absent.protocol
            ].join("|");
            "##,
            "hyperlink URL component setters",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "hyperlinkSetterResult"),
            "https://example.com/player?hostBridge=1#destination|https://example.com/player?hostBridge=1#destination|/player|?hostBridge=1|#destination|https://example.test/new|false|:"
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
    fn top_level_iframe_rect_reuses_container_document_geometry_only() {
        // CSSOM View §6 reports the iframe element's border box from its container Document.
        // HTML §7.3.1.3 makes the iframe's active content Document distinct, so changing that
        // child tree cannot change the already-laid-out embedding box. A mutation of the iframe
        // itself still invalidates and recomputes the box.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            body.innerHTML = `<iframe id="frame" style="width:300px;height:150px"
                srcdoc="<div id='child'>one</div>"></iframe>`;
            globalThis.frameWidthBefore =
                document.getElementById("frame").getBoundingClientRect().width;
            "##,
            "initial iframe geometry",
        )
        .unwrap();
        let (dom, cache) = {
            let state = engine
                .ctx()
                .host_mut::<HostState>()
                .expect("platform HostState remains installed");
            (state.dom.clone(), state.geom_cache.clone())
        };
        assert_eq!(cache.borrow().epoch, dom.borrow().epoch());

        eval(
            &mut engine,
            r##"
            globalThis.detachedForFrameRect = document.createElement("section");
            detachedForFrameRect.innerHTML = "<strong>detached</strong>";
            globalThis.frameWidthAfterDetached =
                document.getElementById("frame").getBoundingClientRect().width;
            "##,
            "detached mutation and container rect read",
        )
        .unwrap();
        assert!(
            cache.borrow().epoch < dom.borrow().epoch(),
            "a disconnected subtree has no box and cannot invalidate the frame"
        );
        eval(
            &mut engine,
            r##"
            document.body.appendChild(detachedForFrameRect);
            globalThis.frameWidthAfterDetachedInsertion =
                document.getElementById("frame").getBoundingClientRect().width;
            "##,
            "connected insertion and container rect read",
        )
        .unwrap();
        assert_eq!(
            cache.borrow().epoch,
            dom.borrow().epoch(),
            "inserting the subtree into the container Document forces layout"
        );

        eval(
            &mut engine,
            r##"
            const frameAfterChildMutation = document.getElementById("frame");
            frameAfterChildMutation.contentDocument.querySelector("#child").textContent = "two";
            "##,
            "child Document mutation",
        )
        .unwrap();
        let child_changes = dom
            .borrow_mut()
            .take_geometry_dirty_targets()
            .expect("child Document mutation remains attributed");
        let child_change_scopes = {
            let dom = dom.borrow();
            child_changes
                .iter()
                .map(|(node, kind)| {
                    (
                        *node,
                        *kind,
                        dom.tag_name(*node).map(str::to_string),
                        dom.frame_owner(*node),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert!(
            child_change_scopes.iter().all(|(_, kind, tag, owner)| {
                owner.is_some()
                    || (*kind == crate::dom::DirtyKind::Content
                        && matches!(tag.as_deref(), Some("iframe" | "frame")))
            }),
            "child mutation escaped its nested Document: {child_change_scopes:?}"
        );
        eval(
            &mut engine,
            r##"
            const frameAfterSecondChildMutation = document.getElementById("frame");
            frameAfterSecondChildMutation.contentDocument.querySelector("#child").textContent = "three";
            globalThis.frameWidthAfterChild =
                frameAfterSecondChildMutation.getBoundingClientRect().width;
            "##,
            "second child Document mutation and container rect read",
        )
        .unwrap();
        assert!(
            cache.borrow().epoch < dom.borrow().epoch(),
            "child Document mutation reused the container Document box map"
        );
        assert!(cache.borrow().top_document_valid);

        eval(
            &mut engine,
            r##"
            const frameAfterContainerMutation = document.getElementById("frame");
            frameAfterContainerMutation.style.width = "410px";
            globalThis.frameWidthAfterContainer =
                frameAfterContainerMutation.getBoundingClientRect().width;
            "##,
            "container Document mutation and iframe rect read",
        )
        .unwrap();
        assert_eq!(cache.borrow().epoch, dom.borrow().epoch());
        assert_eq!(string_value(&mut engine, "frameWidthBefore"), "300");
        assert_eq!(string_value(&mut engine, "frameWidthAfterChild"), "300");
        assert_eq!(string_value(&mut engine, "frameWidthAfterContainer"), "410");
    }

    #[test]
    fn computed_width_and_height_are_rendered_used_values() {
        // CSSOM §9 resolved values: width/height use laid-out values whenever
        // the property applies and the element generates a box. Keep the CSS
        // property box distinct from CSSOM View's border-box rectangle so both
        // content-box and border-box sizing serialize correctly. Stockcharts'
        // fitWidth HOC depends on the first `width:auto` case at mount time.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            html.style.cssText = "margin:0;padding:0";
            body.style.cssText = "margin:0;padding:0";
            body.innerHTML = `
                <div id="auto"><span>fit</span></div>
                <div id="content" style="width:100px;height:20px;padding:10px;border:2px solid"></div>
                <div id="border" style="box-sizing:border-box;width:100px;height:40px;padding:10px;border:2px solid"></div>
                <div id="basis" style="width:400px"><div id="percent" style="width:50%;height:10px"></div></div>
                <span id="inline" style="width:75px">inline</span>
                <div id="hidden" style="display:none;width:50%"></div>`;
            const auto = document.getElementById("auto");
            const content = document.getElementById("content");
            const border = document.getElementById("border");
            const percent = document.getElementById("percent");
            const inline = document.getElementById("inline");
            const hidden = document.getElementById("hidden");
            globalThis.resolvedSizeResult = [
                getComputedStyle(auto).width,
                getComputedStyle(content).width, content.getBoundingClientRect().width,
                getComputedStyle(content).height, content.getBoundingClientRect().height,
                getComputedStyle(border).width, border.getBoundingClientRect().width,
                getComputedStyle(border).height, border.getBoundingClientRect().height,
                getComputedStyle(percent).width,
                getComputedStyle(inline).width,
                getComputedStyle(hidden).width
            ].join("|");
            "##,
            "CSSOM used width and height",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "resolvedSizeResult"),
            "640px|100px|124|20px|44|100px|100|40px|40|200px|75px|50%"
        );
    }

    #[test]
    fn computed_font_size_is_an_absolute_length() {
        // CSSOM §9 exposes the computed value for font-size. CSS Fonts
        // §2.5 defines that computed value as an absolute length, including
        // when the specified value is the initial `medium`, a percentage, or
        // a font-relative length.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            const initial = getComputedStyle(html).fontSize;
            html.style.fontSize = "62.5%";
            body.innerHTML = `<div id="child" style="font-size:1.5em">text</div>`;
            globalThis.computedFontSizeResult = [
                initial,
                getComputedStyle(html).fontSize,
                getComputedStyle(document.getElementById("child")).fontSize
            ].join("|");
            "##,
            "CSSOM computed font-size",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "computedFontSizeResult"),
            "16px|10px|15px"
        );
    }

    #[test]
    fn shadow_host_percentage_width_uses_its_containing_block_content_box() {
        // CSS 2.2 §10.3.3 solves an auto-width normal-flow block from its containing block after
        // subtracting padding. CSS Sizing §5 then resolves a child's 100% width against that
        // content box, including when the child is a custom-element host styled through :host.
        // CSSOM View reports the resulting padding-box width through clientWidth.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            html.style.cssText = "margin:0;padding:0";
            body.style.cssText = "margin:0;padding:0";

            const pane = document.createElement("x-chart-pane");
            pane.style.cssText = "display:block;width:768px";
            body.appendChild(pane);
            const paneShadow = pane.attachShadow({ mode: "open" });
            paneShadow.innerHTML = '<style>' +
                '.main{padding-right:20rem;height:288px}' +
                '.main > *{width:100%;height:100%}' +
                '</style><div class="main"><x-time-chart></x-time-chart></div>';
            const chart = paneShadow.querySelector("x-time-chart");
            chart.attachShadow({ mode: "open" }).innerHTML =
                '<style>:host{display:block !important;position:relative !important}</style>';
            globalThis.shadowHostPercentageWidthResult = [
                getComputedStyle(chart).display,
                getComputedStyle(chart).width,
                chart.getBoundingClientRect().width,
                chart.clientWidth
            ].join("|");
            "##,
            "shadow host percentage width",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "shadowHostPercentageWidthResult"),
            "block|448px|448|448"
        );
    }

    #[test]
    fn dom_rect_interfaces_follow_geometry_level_one() {
        // Geometry Interfaces Module Level 1 §3: constructors/fromRect default
        // missing dictionary members, readonly and mutable rectangles share
        // derived edges, and negative dimensions reverse those edges.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const ro = DOMRectReadOnly.fromRect({ x: 5, y: 7, width: -2, height: -3 });
            const rw = new DOMRect(1, 2, 3, 4);
            rw.x = 9; rw.height = -6;
            const json = rw.toJSON();
            globalThis.domRectResult = [
                ro.left, ro.top, ro.right, ro.bottom,
                rw.x, rw.y, rw.width, rw.height, rw.top, rw.bottom,
                rw instanceof DOMRectReadOnly, SVGRect === DOMRect,
                Object.prototype.toString.call(ro), Object.prototype.toString.call(rw),
                json.x, json.bottom, DOMRect.fromRect().width
            ].join("|");
            "##,
            "Geometry Interfaces DOMRect boundary",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "domRectResult"),
            "3|4|5|7|9|2|3|-6|-4|2|true|true|[object DOMRectReadOnly]|[object DOMRect]|9|2|0"
        );
    }

    #[test]
    fn marquee_interface_reflects_timing_and_controls_render_pause_state() {
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            const marquee = document.createElement("marquee");
            document.appendChild(marquee);
            marquee.direction = "right";
            marquee.scrollAmount = 9;
            marquee.scrollDelay = 20;
            marquee.loop = 3;
            marquee.stop();
            const stopped = marquee.hasAttribute("data-trust-marquee-stopped");
            marquee.start();
            globalThis.marqueeResult = [
                marquee instanceof HTMLMarqueeElement,
                marquee.direction,
                marquee.scrollAmount,
                marquee.scrollDelay,
                marquee.loop,
                stopped,
                marquee.hasAttribute("data-trust-marquee-stopped"),
                Number(marquee.getAttribute("data-trust-marquee-paused-total")) >= 0
            ].join("|");
            "#,
            "marquee interface",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "marqueeResult"),
            "true|right|9|20|3|true|false|true"
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
    fn cache_storage_follows_query_vary_and_deleted_cache_lifetime_algorithms() {
        // Service Workers §5.4–§5.5: Cache matching excludes fragments, honors
        // queries and Vary by default, returns fresh Response objects, and a
        // deleted name does not invalidate an already-referenced Cache object.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            globalThis.cacheStorageResult = "pending";
            (async function () {
                const sameObject = caches === caches;
                const cache = await caches.open("conformance");
                const stored = new Response("hello", {
                    headers: { "content-type": "text/plain", "vary": "accept-language" }
                });
                await cache.put(new Request("https://example.com/item?q=1#original", {
                    headers: { "accept-language": "en" }
                }), stored);

                const hit = await cache.match(new Request("https://example.com/item?q=1#other", {
                    headers: { "accept-language": "en" }
                }));
                const miss = await cache.match(new Request("https://example.com/item?q=1", {
                    headers: { "accept-language": "fr" }
                }));
                const ignored = await cache.match(new Request("https://example.com/item?q=2", {
                    headers: { "accept-language": "fr" }
                }), { ignoreSearch: true, ignoreVary: true });
                const keys = await cache.keys();
                const namesBefore = await caches.keys();
                const removedName = await caches.delete("conformance");
                const retained = await cache.match(new Request("https://example.com/item?q=1", {
                    headers: { "accept-language": "en" }
                }));
                const replacement = await caches.open("conformance");
                const replacementMiss = await replacement.match("https://example.com/item?q=1");
                const firstDelete = await cache.delete("https://example.com/item?q=1", {
                    ignoreVary: true
                });
                const secondDelete = await cache.delete("https://example.com/item?q=1", {
                    ignoreVary: true
                });

                cacheStorageResult = [
                    typeof Cache, typeof CacheStorage,
                    cache instanceof Cache, sameObject,
                    await hit.text(), miss === undefined,
                    await ignored.text(), keys.length,
                    keys[0].url.endsWith("#original"),
                    namesBefore.join(","), removedName,
                    await retained.text(), replacementMiss === undefined,
                    firstDelete, secondDelete, stored.bodyUsed
                ].join("|");
            })().catch(function (error) {
                cacheStorageResult = "ERROR:" + error.name + ":" + error.message +
                    (error.stack ? "\n" + error.stack : "");
            });
            "##,
            "CacheStorage conformance",
        )
        .unwrap();
        run_microtask_checkpoint(&mut engine);

        assert_eq!(
            string_value(&mut engine, "cacheStorageResult"),
            "function|function|true|true|hello|true|hello|1|true|conformance|true|hello|true|true|false|true"
        );
    }

    #[test]
    fn dataset_exposes_live_enumerable_dom_string_map_properties() {
        // WHATWG HTML §3.2.6.6: DOMStringMap's supported property names are
        // derived from the current data-* attribute list, preserve that order,
        // and are enumerable own named properties. GitLab enumerates dataset
        // before parsing its JSON-valued programmingLanguages boot attribute.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const element = document.createElement("div");
            element.setAttribute("data-programming-languages", '[{"id":30,"name":"C"}]');
            element.setAttribute("data-user-id", "17");
            const dataset = element.dataset;
            const copied = Object.assign({}, dataset);
            const parsed = JSON.parse(copied.programmingLanguages);
            const firstKeys = Object.keys(dataset).join(",");
            const descriptor = Object.getOwnPropertyDescriptor(dataset, "userId");
            element.setAttribute("data-later-value", "live");
            dataset.fooBar = 42;
            const reflected = element.getAttribute("data-foo-bar");
            delete dataset.userId;
            let invalidName = "";
            try { dataset["bad-name"] = "x"; } catch (error) { invalidName = error.name; }
            let symbolValue = "";
            try { dataset.symbol = Symbol("x"); } catch (error) { symbolValue = error.name; }
            globalThis.datasetEnumerationResult = [
                dataset instanceof DOMStringMap,
                firstKeys,
                parsed[0].name,
                descriptor.enumerable,
                descriptor.configurable,
                descriptor.writable,
                dataset.laterValue,
                Object.keys(dataset).join(","),
                reflected,
                element.hasAttribute("data-user-id"),
                typeof dataset.toString,
                invalidName,
                symbolValue
            ].join("|");
            "##,
            "DOMStringMap supported named properties",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "datasetEnumerationResult"),
            "true|programmingLanguages,userId|C|true|true|true|live|programmingLanguages,laterValue,fooBar|42|false|function|SyntaxError|TypeError"
        );
    }

    #[test]
    fn indexed_db_orders_upgrade_requests_transactions_and_cursor_iteration() {
        // Indexed Database 3 §§2.7.1, 4.1, 4.3–4.5, 4.9, and 5.1/5.6/5.7:
        // an open request upgrades before succeeding; transaction requests and
        // cursor continuations complete in insertion order; request callbacks
        // reactivate their transaction; and stored values are structured
        // clones rather than aliases to author objects.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            globalThis.indexedDbResult = "pending";
            const idbLog = [];
            const open = indexedDB.open("trust-indexed-db-conformance", 1);
            let pendingThrows = false;
            try { void open.result; }
            catch (error) { pendingThrows = error.name === "InvalidStateError"; }

            open.onupgradeneeded = function (event) {
                idbLog.push("upgrade:" + event.oldVersion + ":" + event.newVersion + ":" +
                    (event.target === open) + ":" + (open.transaction.mode === "versionchange"));
                const store = open.result.createObjectStore("items");
                const original = { nested: { value: 1 }, bytes: new Uint8Array([3, 4]) };
                store.put(original, "b");
                original.nested.value = 88;
                original.bytes[0] = 9;
                store.put({ nested: { value: 2 } }, "a");
                open.transaction.oncomplete = function () { idbLog.push("upgrade-complete"); };
            };
            open.onerror = function () {
                indexedDbResult = "OPEN-ERROR:" + open.error.name + ":" + open.error.message;
            };
            open.onsuccess = function () {
                idbLog.push("open-success:" + open.result.objectStoreNames.contains("items"));
                const db = open.result;
                const transaction = db.transaction("items", "readonly", { durability: "relaxed" });
                const store = transaction.objectStore("items");
                const first = store.get("b");
                first.onsuccess = function (event) {
                    const sameResultObject = first.result === first.result;
                    idbLog.push("get:" + first.result.nested.value + ":" + first.result.bytes[0] +
                        ":" + sameResultObject + ":" + (event.target === first));
                    first.result.nested.value = 99;
                    const again = store.get("b");
                    again.onsuccess = function () { idbLog.push("again:" + again.result.nested.value); };
                };
                const keys = store.getAllKeys();
                keys.onsuccess = function () { idbLog.push("keys:" + keys.result.join(",")); };
                const cursorRequest = store.openCursor();
                cursorRequest.onsuccess = function () {
                    const cursor = cursorRequest.result;
                    if (cursor) {
                        idbLog.push("cursor:" + cursor.key + ":" + cursor.value.nested.value);
                        cursor.continue();
                    } else idbLog.push("cursor:end");
                };
                transaction.oncomplete = function () {
                    let inactive = false;
                    try { store.get("a"); }
                    catch (error) { inactive = error.name === "TransactionInactiveError"; }
                    idbLog.push("read-complete:" + inactive);
                    db.close();
                    const remove = indexedDB.deleteDatabase("trust-indexed-db-conformance");
                    remove.onsuccess = function (event) {
                        idbLog.push("delete:" + event.oldVersion + ":" + event.newVersion);
                        indexedDbResult = pendingThrows + "|" + idbLog.join("|");
                    };
                };
            };
            "##,
            "IndexedDB transaction algorithms",
        )
        .unwrap();

        for _ in 0..128 {
            run_microtask_checkpoint(&mut engine);
            let has_task = engine_call_trust_method(&mut engine, "hasPlatformTask", &[])
                .map(|value| engine.ctx().to_boolean(&value))
                .unwrap_or(false);
            if !has_task {
                break;
            }
            assert!(
                engine_call_trust_method(&mut engine, "runPlatformTask", &[])
                    .is_ok_and(|value| engine.ctx().to_boolean(&value))
            );
        }
        run_microtask_checkpoint(&mut engine);

        assert_eq!(
            string_value(&mut engine, "indexedDbResult"),
            "true|upgrade:0:1:true:true|upgrade-complete|open-success:true|get:1:3:true:true|keys:a,b|cursor:a:2|again:1|cursor:b:1|cursor:end|read-complete:true|delete:1:null"
        );
        assert_eq!(string_value(&mut engine, "__trust.takeErrors()"), "");
    }

    #[test]
    fn indexed_db_request_events_follow_parent_path_and_cancellation_rules() {
        // Indexed Database 3 §§2.7.1, 2.8, and 5.9–5.10, together with
        // WHATWG DOM §2.9: a request's event parent is its transaction, whose
        // parent is the database connection. Error events capture and bubble
        // over that path, reactivate the transaction for every listener, and
        // a canceled error allows later requests to complete. Success events
        // traverse the capture path but do not bubble.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            globalThis.indexedDbEventResult = "pending";
            const eventLog = [];
            const open = indexedDB.open("trust-indexed-db-event-conformance", 1);
            open.onupgradeneeded = function () {
                const store = open.result.createObjectStore("items");
                store.createIndex("code", "code", { unique: true });
                store.put({ code: "one", value: 1 }, 1);
            };
            open.onerror = function () {
                indexedDbEventResult = "OPEN-ERROR:" + open.error.name;
            };
            open.onsuccess = function () {
                const db = open.result;
                const transaction = db.transaction("items", "readwrite");
                const store = transaction.objectStore("items");
                let duplicate;
                let recovery;
                let unexpectedSuccessBubble = 0;
                function record(label, event, current, target) {
                    const path = event.composedPath();
                    eventLog.push(label + ":" + event.eventPhase + ":" +
                        (event.target === target) + ":" +
                        (event.currentTarget === current) + ":" + event.isTrusted + ":" +
                        (path.length === 3 && path[0] === target &&
                            path[1] === transaction && path[2] === db));
                }

                db.addEventListener("error", function (event) {
                    record("db-error-capture", event, db, duplicate);
                }, true);
                transaction.addEventListener("error", function (event) {
                    record("transaction-error-capture", event, transaction, duplicate);
                }, true);
                transaction.addEventListener("error", function (event) {
                    record("transaction-error-bubble", event, transaction, duplicate);
                });
                db.addEventListener("error", function (event) {
                    record("db-error-bubble", event, db, duplicate);
                    event.preventDefault();
                });
                transaction.addEventListener("success", function () {
                    unexpectedSuccessBubble++;
                });
                db.addEventListener("success", function () {
                    unexpectedSuccessBubble++;
                });

                duplicate = store.put({ code: "one", value: 2 }, 2);
                duplicate.onerror = function (event) {
                    record("request-error", event, duplicate, duplicate);
                    eventLog.push("error:" + duplicate.error.name);
                    // Request event dispatch makes the transaction active, so
                    // this recovery request must be accepted from the handler.
                    recovery = store.get(1);
                    db.addEventListener("success", function (successEvent) {
                        if (successEvent.target === recovery)
                            record("db-success-capture", successEvent, db, recovery);
                    }, true);
                    transaction.addEventListener("success", function (successEvent) {
                        if (successEvent.target === recovery)
                            record("transaction-success-capture", successEvent,
                                transaction, recovery);
                    }, true);
                    recovery.onsuccess = function (successEvent) {
                        record("request-success", successEvent, recovery, recovery);
                        eventLog.push("recovery:" + recovery.result.code);
                    };
                };
                transaction.onabort = function () {
                    indexedDbEventResult = "UNEXPECTED-ABORT:" + transaction.error.name;
                };
                transaction.oncomplete = function (event) {
                    eventLog.push("complete:" + event.eventPhase + ":" +
                        event.isTrusted + ":" + unexpectedSuccessBubble);
                    db.close();
                    const remove = indexedDB.deleteDatabase(
                        "trust-indexed-db-event-conformance");
                    remove.onsuccess = function () {
                        indexedDbEventResult = eventLog.join("|");
                    };
                };
            };
            "##,
            "IndexedDB request event propagation algorithms",
        )
        .unwrap();

        for _ in 0..192 {
            run_microtask_checkpoint(&mut engine);
            let has_task = engine_call_trust_method(&mut engine, "hasPlatformTask", &[])
                .map(|value| engine.ctx().to_boolean(&value))
                .unwrap_or(false);
            if !has_task {
                break;
            }
            assert!(
                engine_call_trust_method(&mut engine, "runPlatformTask", &[])
                    .is_ok_and(|value| engine.ctx().to_boolean(&value))
            );
        }
        run_microtask_checkpoint(&mut engine);

        assert_eq!(
            string_value(&mut engine, "indexedDbEventResult"),
            "db-error-capture:1:true:true:true:true|transaction-error-capture:1:true:true:true:true|request-error:2:true:true:true:true|error:ConstraintError|transaction-error-bubble:3:true:true:true:true|db-error-bubble:3:true:true:true:true|db-success-capture:1:true:true:true:true|transaction-success-capture:1:true:true:true:true|request-success:2:true:true:true:true|recovery:one|complete:2:true:0"
        );
        assert_eq!(string_value(&mut engine, "__trust.takeErrors()"), "");
    }

    #[test]
    fn indexed_db_listener_exception_aborts_with_abort_error() {
        // Indexed Database 3 §§5.9–5.10 and WHATWG DOM §2.9 "inner invoke":
        // the dispatch algorithm reports listener exceptions through its
        // IndexedDB-only legacy output flag. That flag aborts the transaction
        // with AbortError, even when the request itself succeeded.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            globalThis.indexedDbListenerExceptionResult = "pending";
            const exceptionLog = [];
            const open = indexedDB.open(
                "trust-indexed-db-listener-exception-conformance", 1);
            open.onupgradeneeded = function () {
                const store = open.result.createObjectStore("items");
                store.put({ value: 1 }, 1);
            };
            open.onsuccess = function () {
                const db = open.result;
                const transaction = db.transaction("items");
                const request = transaction.objectStore("items").get(1);
                transaction.oncomplete = function () {
                    indexedDbListenerExceptionResult = "UNEXPECTED-COMPLETE";
                };
                transaction.onabort = function (event) {
                    exceptionLog.push("transaction-abort:" + event.eventPhase + ":" +
                        (event.target === transaction) + ":" +
                        (event.currentTarget === transaction) + ":" + event.isTrusted + ":" +
                        transaction.error.name);
                    db.close();
                    const remove = indexedDB.deleteDatabase(
                        "trust-indexed-db-listener-exception-conformance");
                    remove.onsuccess = function () {
                        exceptionLog.push("delete");
                        indexedDbListenerExceptionResult = exceptionLog.join("|");
                    };
                };
                db.addEventListener("abort", function (event) {
                    exceptionLog.push("database-abort:" + event.eventPhase + ":" +
                        (event.target === transaction) + ":" +
                        (event.currentTarget === db) + ":" + event.isTrusted + ":" +
                        transaction.error.name);
                });
                request.onsuccess = function () {
                    throw new Error("idb-listener-boom");
                };
            };
            open.onerror = function () {
                indexedDbListenerExceptionResult = "OPEN-ERROR:" + open.error.name;
            };
            "##,
            "IndexedDB listener exception abort algorithm",
        )
        .unwrap();

        for _ in 0..160 {
            run_microtask_checkpoint(&mut engine);
            let has_task = engine_call_trust_method(&mut engine, "hasPlatformTask", &[])
                .map(|value| engine.ctx().to_boolean(&value))
                .unwrap_or(false);
            if !has_task {
                break;
            }
            assert!(
                engine_call_trust_method(&mut engine, "runPlatformTask", &[])
                    .is_ok_and(|value| engine.ctx().to_boolean(&value))
            );
        }
        run_microtask_checkpoint(&mut engine);

        assert_eq!(
            string_value(&mut engine, "indexedDbListenerExceptionResult"),
            "transaction-abort:2:true:true:true:AbortError|database-abort:3:true:true:true:AbortError|delete"
        );
        assert!(
            string_value(&mut engine, "__trust.takeErrors()")
                .contains("success handler: idb-listener-boom")
        );
    }

    #[test]
    fn indexed_db_connection_queue_resumes_after_close_pending_transaction() {
        // Indexed Database 3 §§2.8.2 and 5.1–5.3: open/delete operations for
        // one storage key and name are serialized. An upgrade sends
        // versionchange, reports blocked while an old close-pending connection
        // still has a live transaction, and resumes only when that connection
        // is actually closed.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            globalThis.indexedDbConnectionQueueResult = "pending";
            const connectionLog = [];
            const name = "trust-indexed-db-connection-queue-conformance";
            const first = indexedDB.open(name, 1);
            first.onupgradeneeded = function () {
                first.result.createObjectStore("items");
            };
            first.onsuccess = function () {
                const firstDb = first.result;
                connectionLog.push("first-success:" + firstDb.version);
                const hold = firstDb.transaction("items", "readwrite");
                hold.objectStore("items").put("held", 1);
                hold.oncomplete = function () { connectionLog.push("hold-complete"); };
                firstDb.onversionchange = function (event) {
                    connectionLog.push("versionchange:" + event.oldVersion + ":" +
                        event.newVersion + ":" + event.isTrusted);
                    // close() marks the connection close-pending immediately,
                    // but it remains open until `hold` finishes.
                    firstDb.close();
                };

                const second = indexedDB.open(name, 2);
                second.onblocked = function (event) {
                    connectionLog.push("blocked:" + event.oldVersion + ":" +
                        event.newVersion + ":" + event.isTrusted);
                };
                second.onupgradeneeded = function (event) {
                    connectionLog.push("upgrade:" + event.oldVersion + ":" +
                        event.newVersion);
                    second.result.createObjectStore("new-store");
                };
                second.onsuccess = function () {
                    connectionLog.push("second-success:" + second.result.version);
                    second.result.close();
                };
                second.onerror = function () {
                    indexedDbConnectionQueueResult = "SECOND-ERROR:" + second.error.name;
                };

                // This request is queued behind the blocked upgrade and must
                // observe/delete version 2, not race version 1.
                const remove = indexedDB.deleteDatabase(name);
                remove.onsuccess = function (event) {
                    connectionLog.push("delete:" + event.oldVersion + ":" +
                        event.newVersion + ":" + event.isTrusted);
                    indexedDbConnectionQueueResult = connectionLog.join("|");
                };
                remove.onerror = function () {
                    indexedDbConnectionQueueResult = "DELETE-ERROR:" + remove.error.name;
                };
            };
            first.onerror = function () {
                indexedDbConnectionQueueResult = "FIRST-ERROR:" + first.error.name;
            };
            "##,
            "IndexedDB connection queue and close blocking algorithms",
        )
        .unwrap();

        for _ in 0..256 {
            run_microtask_checkpoint(&mut engine);
            let has_task = engine_call_trust_method(&mut engine, "hasPlatformTask", &[])
                .map(|value| engine.ctx().to_boolean(&value))
                .unwrap_or(false);
            if !has_task {
                break;
            }
            assert!(
                engine_call_trust_method(&mut engine, "runPlatformTask", &[])
                    .is_ok_and(|value| engine.ctx().to_boolean(&value))
            );
        }
        run_microtask_checkpoint(&mut engine);

        assert_eq!(
            string_value(&mut engine, "indexedDbConnectionQueueResult"),
            "first-success:1|versionchange:1:2:true|blocked:1:2:true|hold-complete|upgrade:1:2|second-success:2|delete:2:null:true"
        );
        assert_eq!(string_value(&mut engine, "__trust.takeErrors()"), "");
    }

    #[test]
    fn indexed_db_indexes_follow_secondary_order_multi_entry_and_unique_constraints() {
        // Indexed Database 3 §§4.6, 6.1, 6.3, and 6.7: index rows are sorted
        // by index key then primary key; multiEntry flattens only the outer
        // array and removes duplicates; unique directions select one primary
        // row per index key; and a failed unique write rolls back that request.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            globalThis.indexedDbIndexResult = "pending";
            const indexLog = [];
            const open = indexedDB.open("trust-indexed-db-index-conformance", 1);
            open.onupgradeneeded = function () {
                const store = open.result.createObjectStore("items");
                store.createIndex("category", "category");
                store.createIndex("tags", "tags", { multiEntry: true });
                store.createIndex("code", "code", { unique: true });
                store.put({ category: "x", tags: ["a", "b", "a"], code: "one" }, 1);
                store.put({ category: "x", tags: ["b", "c"], code: "two" }, 2);
                store.put({ category: "y", tags: ["c"], code: "three" }, 3);
            };
            open.onerror = function () {
                indexedDbIndexResult = "OPEN-ERROR:" + open.error.name;
            };
            open.onsuccess = function () {
                const db = open.result;
                const read = db.transaction("items");
                const store = read.objectStore("items");
                const category = store.index("category");
                const tags = store.index("tags");
                const first = category.get("x");
                first.onsuccess = () => indexLog.push("first:" + first.result.code);
                const firstKey = category.getKey("x");
                firstKey.onsuccess = () => indexLog.push("first-key:" + firstKey.result);
                const keys = category.getAllKeys("x");
                keys.onsuccess = () => indexLog.push("keys:" + keys.result.join(","));
                const tagKeys = tags.getAllKeys("b");
                tagKeys.onsuccess = () => indexLog.push("tags:" + tagKeys.result.join(","));
                const count = tags.count(IDBKeyRange.bound("b", "c"));
                count.onsuccess = () => indexLog.push("count:" + count.result);
                const cursorKeys = [];
                const cursorRequest = category.openCursor();
                cursorRequest.onsuccess = function () {
                    const cursor = cursorRequest.result;
                    if (cursor) {
                        cursorKeys.push(cursor.key + "/" + cursor.primaryKey);
                        cursor.continue();
                    } else indexLog.push("cursor:" + cursorKeys.join(","));
                };
                const uniqueKeys = [];
                const uniqueRequest = category.openKeyCursor(null, "nextunique");
                uniqueRequest.onsuccess = function () {
                    const cursor = uniqueRequest.result;
                    if (cursor) {
                        uniqueKeys.push(cursor.key + "/" + cursor.primaryKey);
                        cursor.continue();
                    } else indexLog.push("unique:" + uniqueKeys.join(","));
                };
                read.oncomplete = function () {
                    const write = db.transaction("items", "readwrite");
                    const writable = write.objectStore("items");
                    const duplicate = writable.put({ category: "z", tags: [], code: "one" }, 4);
                    duplicate.onerror = function (event) {
                        indexLog.push("constraint:" + duplicate.error.name);
                        event.preventDefault();
                        const absent = writable.get(4);
                        absent.onsuccess = () => indexLog.push("rollback:" + (absent.result === undefined));
                    };
                    write.oncomplete = function () {
                        db.close();
                        const remove = indexedDB.deleteDatabase("trust-indexed-db-index-conformance");
                        remove.onsuccess = function () {
                            indexedDbIndexResult = indexLog.join("|");
                        };
                    };
                };
            };
            "##,
            "IndexedDB index algorithms",
        )
        .unwrap();

        for _ in 0..192 {
            run_microtask_checkpoint(&mut engine);
            let has_task = engine_call_trust_method(&mut engine, "hasPlatformTask", &[])
                .map(|value| engine.ctx().to_boolean(&value))
                .unwrap_or(false);
            if !has_task {
                break;
            }
            assert!(
                engine_call_trust_method(&mut engine, "runPlatformTask", &[])
                    .is_ok_and(|value| engine.ctx().to_boolean(&value))
            );
        }
        run_microtask_checkpoint(&mut engine);

        let errors = string_value(&mut engine, "__trust.takeErrors()");
        assert_eq!(
            string_value(&mut engine, "indexedDbIndexResult"),
            "first:one|first-key:1|keys:1,2|tags:1,2|count:4|unique:x/1,y/3|cursor:x/1,x/2,y/3|constraint:ConstraintError|rollback:true",
            "errors={errors}"
        );
        assert_eq!(errors, "");
    }

    #[test]
    fn minimal_boundary_boots_the_real_prelude() {
        let mut engine = platform_engine();
        let node_type = eval_value(&mut engine, "document.nodeType", "node type").unwrap();
        assert_eq!(node_type.as_num_opt(), Some(9.0));
        assert_eq!(crate::dom::DOCUMENT, 0);
    }

    #[test]
    fn document_base_uri_resolves_relative_urls_for_documents_and_nodes() {
        // DOM §4.4 `Node.baseURI` and HTML §2.4.3 document base URLs. A <base>
        // element changes the base used by URL APIs without changing
        // document.URL, and the value is exposed on both the Document and its
        // descendant nodes.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            const html = document.createElement("html");
            const head = document.createElement("head");
            const body = document.createElement("body");
            const base = document.createElement("base");
            base.setAttribute("href", "/static/");
            const target = document.createElement("div");
            document.appendChild(html);
            html.appendChild(head); html.appendChild(body);
            head.appendChild(base); body.appendChild(target);
            globalThis.baseUriResult = [
                document.URL,
                document.baseURI,
                target.baseURI,
                new URL("bundle.js", document.baseURI).href
            ].join("|");
            head.innerHTML = '<base href="/dynamic/">';
            globalThis.dynamicBaseUriResult = [
                document.baseURI,
                target.baseURI,
                new URL("bundle.js", target.baseURI).href
            ].join("|");
            "#,
            "document base URL",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "baseUriResult"),
            "https://example.com/|https://example.com/static/|https://example.com/static/|https://example.com/static/bundle.js"
        );
        assert_eq!(
            string_value(&mut engine, "dynamicBaseUriResult"),
            "https://example.com/dynamic/|https://example.com/dynamic/|https://example.com/dynamic/bundle.js"
        );
    }

    #[test]
    fn iframe_src_is_always_resolved_against_its_node_document() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        // HTML §4.8.5's shared iframe-attribute processing steps parse `src`
        // relative to the iframe element's node Document. A callback running
        // with the child Window's settings object must not reinterpret the
        // embedding element's relative URL against the child Document and
        // accidentally navigate the child a second time.
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
            for _ in 0..2 {
                let Ok(Ok((mut socket, _))) =
                    tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
                else {
                    break;
                };
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let target = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_owned();
                request_tx.send(target.clone()).unwrap();
                let body = if target == "/speedometer/resources/angular/index.html" {
                    r#"<html><head><base href="child-assets/"></head><body id="original"><script>
                        Promise.resolve().then(function () {
                            document.body.setAttribute("data-bases", [
                                frameElement.baseURI,
                                frameElement.ownerDocument.baseURI,
                                document.body.baseURI
                            ].join("|"));
                            frameElement.contentDocument;
                        });
                    </script></body></html>"#
                } else {
                    r#"<body id="wrong-base-replacement"></body>"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let page = url::Url::parse(&format!("http://{address}/speedometer/index.html")).unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        let (task_tx, _task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page.clone(), runtime.handle().clone(), cache, task_tx);
        let mut engine = configured_engine(state, page.as_str());

        eval(
            &mut engine,
            r#"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                frame.setAttribute("src", "resources/angular/index.html");
                body.appendChild(frame);
                __trust.hydrateFrames();
                globalThis.relativeFrame = frame;
            "#,
            "relative iframe src setup",
        )
        .unwrap();
        run_microtask_checkpoint(&mut engine);

        assert_eq!(
            string_value(&mut engine, "relativeFrame.contentDocument.body.id"),
            "original"
        );
        assert_eq!(
            string_value(
                &mut engine,
                "relativeFrame.contentDocument.body.getAttribute('data-bases')"
            ),
            format!(
                "http://{address}/speedometer/index.html|http://{address}/speedometer/index.html|http://{address}/speedometer/resources/angular/child-assets/"
            )
        );
        assert_eq!(
            request_rx.try_iter().collect::<Vec<_>>(),
            vec![String::from("/speedometer/resources/angular/index.html")]
        );
    }

    #[test]
    fn iframe_cross_document_navigation_replaces_the_active_window_event_target() {
        // HTML §7.2.3 gives a browsing context a stable WindowProxy whose
        // [[Window]] changes on ordinary cross-document navigation. Event
        // listeners and event-handler IDL attributes belong to that Window;
        // they must not survive when a benchmark reuses one iframe element for
        // a different application Document.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                frame.srcdoc = '<body><script>' +
                    'window.addEventListener("probe", function () {' +
                    'document.body.setAttribute("data-old-listener", "yes"); });' +
                    'window.onload = function () {};' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();

                frame.srcdoc = '<body><script>' +
                    'const inheritedOnload = window.onload !== null;' +
                    'window.addEventListener("probe", function () {' +
                    'document.body.setAttribute("data-new-listener", "yes"); });' +
                    'window.dispatchEvent(new Event("probe"));' +
                    'document.body.setAttribute("data-result", [' +
                    'inheritedOnload,' +
                    'document.body.getAttribute("data-old-listener"),' +
                    'document.body.getAttribute("data-new-listener")].join("|"));' +
                    '<\/script></body>';
                __trust.hydrateFrames();
                globalThis.reusedFrame = frame;
            "##,
            "iframe Window replacement",
        )
        .unwrap();

        assert_eq!(
            string_value(
                &mut engine,
                "reusedFrame.contentDocument.body.getAttribute('data-result')"
            ),
            "false||yes"
        );
    }

    #[test]
    fn bytecode_repeated_iframe_navigation_keeps_the_active_property_key_sound() {
        // A megamorphic property cache must not use a chunk-owned string address
        // as its sole key. Each navigation destroys the child bytecode chunk, so
        // the allocator may reuse that address for a different property name.
        // Keep this stress case in the bytecode tier: the reference interpreter
        // does not exercise the cache and therefore cannot protect this path.
        let mut engine = platform_engine();
        engine.set_tier(Tier::Bytecode);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                body.appendChild(frame);
                let navigations = 0;
                function navigateAgain() {
                    frame.srcdoc = '<body><script>' +
                        'requestAnimationFrame(function () { document.body.dataset.ready = "yes"; });' +
                        '<\/script></body>';
                    __trust.hydrateFrames();
                    navigations++;
                    if (navigations < 120) requestAnimationFrame(navigateAgain);
                    else frame.contentDocument.body.dataset.done = String(navigations);
                }
                requestAnimationFrame(navigateAgain);
                globalThis.repeatedNavigationFrame = frame;
            "##,
            "bytecode repeated iframe navigation setup",
        )
        .unwrap();

        for _ in 0..300 {
            let deadline = call_trust_method(&mut engine, "nextDeadline", &[]);
            let Some(deadline) = deadline.as_num_opt() else {
                break;
            };
            call_trust_method(&mut engine, "tickTo", &[Value::Num(deadline)]);
            if string_value(
                &mut engine,
                "String(repeatedNavigationFrame.contentDocument.body.dataset.done)",
            ) == "120"
            {
                break;
            }
        }
        assert_eq!(
            string_value(
                &mut engine,
                "String(repeatedNavigationFrame.contentDocument.body.dataset.done)",
            ),
            "120"
        );
        assert_eq!(string_value(&mut engine, "__trust.takeErrors()"), "");
    }

    #[test]
    fn iframe_cross_document_navigation_creates_a_fresh_custom_element_registry() {
        // HTML §4.13.4: Window.customElements returns its associated
        // Document's registry, and every replacement Window/Document is
        // created with a new registry. The same name therefore can be defined
        // by two successive applications, while a duplicate in either one
        // still throws NotSupportedError.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                frame.srcdoc = '<body><x-navigation-registry></x-navigation-registry><script>' +
                    'class FirstDefinition extends HTMLElement {' +
                    'connectedCallback() { this.setAttribute("data-definition", "first"); }}' +
                    'customElements.define("x-navigation-registry", FirstDefinition);' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();
                const firstRegistry = frame.contentWindow.customElements;
                const firstConstructor = firstRegistry.get("x-navigation-registry");

                frame.srcdoc = '<body><x-navigation-registry></x-navigation-registry><script>' +
                    'class SecondDefinition extends HTMLElement {' +
                    'connectedCallback() { this.setAttribute("data-definition", "second"); }}' +
                    'customElements.define("x-navigation-registry", SecondDefinition);' +
                    'try {' +
                    'customElements.define("x-navigation-registry", class extends HTMLElement {});' +
                    '} catch (error) { document.body.setAttribute("data-duplicate-error", error.name); }' +
                    '<\/script></body>';
                __trust.hydrateFrames();
                const secondRegistry = frame.contentWindow.customElements;
                const secondConstructor = secondRegistry.get("x-navigation-registry");
                const element = frame.contentDocument.querySelector("x-navigation-registry");
                globalThis.customElementRegistryNavigationResult = [
                    firstRegistry !== secondRegistry,
                    firstRegistry.get("x-navigation-registry") === firstConstructor,
                    secondConstructor !== firstConstructor,
                    element.getAttribute("data-definition"),
                    frame.contentDocument.body.getAttribute("data-duplicate-error"),
                    typeof firstConstructor,
                    firstConstructor && firstConstructor.name,
                    typeof secondConstructor,
                    secondConstructor && secondConstructor.name
                ].join("|");
            "##,
            "iframe custom-element registry replacement",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "customElementRegistryNavigationResult"),
            "true|true|true|second|NotSupportedError|function|FirstDefinition|function|SecondDefinition"
        );
    }

    #[test]
    fn replacing_an_iframe_element_creates_a_fresh_custom_element_registry() {
        // HTML §4.8.5 iframe removing steps destroy the old child navigable;
        // inserting a different iframe creates a new child navigable. Its
        // initial Window and the Window created for its first application
        // Document therefore cannot inherit the removed iframe's registry.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);

                const firstFrame = document.createElement("iframe");
                firstFrame.srcdoc = '<body><x-recreated-frame></x-recreated-frame><script>' +
                    'class FirstFrameDefinition extends HTMLElement {' +
                    'connectedCallback() { this.setAttribute("data-definition", "first"); }}' +
                    'customElements.define("x-recreated-frame", FirstFrameDefinition);' +
                    '<\/script></body>';
                body.appendChild(firstFrame);
                __trust.hydrateFrames();
                const firstRegistry = firstFrame.contentWindow.customElements;
                const firstElement = firstFrame.contentDocument.querySelector("x-recreated-frame");
                const firstDefinition = firstElement.getAttribute("data-definition");
                body.removeChild(firstFrame);

                const secondFrame = document.createElement("iframe");
                secondFrame.srcdoc = '<body><x-recreated-frame></x-recreated-frame><script>' +
                    'class SecondFrameDefinition extends HTMLElement {' +
                    'connectedCallback() { this.setAttribute("data-definition", "second"); }}' +
                    'customElements.define("x-recreated-frame", SecondFrameDefinition);' +
                    '<\/script></body>';
                body.insertBefore(secondFrame, body.firstChild);
                __trust.hydrateFrames();
                const secondRegistry = secondFrame.contentWindow.customElements;
                const secondElement = secondFrame.contentDocument.querySelector("x-recreated-frame");
                globalThis.recreatedIframeRegistryResult = [
                    firstDefinition,
                    firstRegistry !== secondRegistry,
                    firstRegistry.get("x-recreated-frame").name,
                    secondRegistry.get("x-recreated-frame").name,
                    secondElement.getAttribute("data-definition")
                ].join("|");
            "##,
            "recreated iframe custom-element registry",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "recreatedIframeRegistryResult"),
            "first|true|FirstFrameDefinition|SecondFrameDefinition|second"
        );
    }

    #[test]
    fn iframe_initial_about_blank_has_a_realm_and_reuses_its_window_once() {
        // HTML §7.3.2.1 creates the child browsing context's Realm, Window,
        // environment settings object, and populated initial about:blank
        // Document during iframe post-connection steps. HTML §7.5.1 then
        // reuses that Window for the first same-origin navigation while
        // replacing its Document.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                let initialLoadCount = 0;
                frame.onload = function () { initialLoadCount++; };
                const detachedWindowIsNull = frame.contentWindow === null;
                const detachedDocumentIsNull = frame.contentDocument === null;
                body.appendChild(frame);

                const initialWindow = frame.contentWindow;
                const initialDocument = frame.contentDocument;
                initialWindow.initialNavigationMarker = "preserved";

                frame.setAttribute("srcdoc", '<body><script>' +
                    'document.body.setAttribute("data-marker", initialNavigationMarker);' +
                    '<\/script></body>');
                __trust.hydrateFrames();

                globalThis.initialAboutBlankRealmResult = [
                    detachedWindowIsNull,
                    detachedDocumentIsNull,
                    initialLoadCount,
                    initialWindow !== window,
                    initialWindow.Array.prototype !== Array.prototype,
                    initialDocument !== document,
                    initialDocument.defaultView === initialWindow,
                    frame.contentWindow === initialWindow,
                    frame.contentDocument !== initialDocument,
                    frame.contentDocument.body.getAttribute("data-marker")
                ].join("|");
            "##,
            "iframe initial about:blank Realm",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "initialAboutBlankRealmResult"),
            "true|true|1|true|true|true|true|true|true|preserved"
        );
    }

    #[test]
    fn iframe_windows_do_not_share_writable_platform_globals_across_navigation() {
        // HTML §7.5.1 creates a new Realm and Window for an ordinary
        // cross-document navigation. Writable Window properties patched by
        // one application (Zone.js wraps timers and observer constructors)
        // therefore cannot replace the next application's platform globals or
        // the embedding top Window's globals.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                const topMutationObserver = MutationObserver;
                const topSetTimeout = setTimeout;

                frame.srcdoc = '<body><script>' +
                    'window.MutationObserver = function PatchedObserver() {};' +
                    'window.setTimeout = function patchedTimeout() { return 919; };' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();
                const patchedMutationObserver = frame.contentWindow.MutationObserver;
                const patchedSetTimeout = frame.contentWindow.setTimeout;

                frame.srcdoc = '<body><script>' +
                    'const observer = new MutationObserver(function () {});' +
                    'document.body.setAttribute("data-observe-type", typeof observer.observe);' +
                    '<\/script></body>';
                __trust.hydrateFrames();
                globalThis.windowPlatformGlobalNavigationResult = [
                    frame.contentWindow.MutationObserver !== patchedMutationObserver,
                    frame.contentWindow.setTimeout !== patchedSetTimeout,
                    frame.contentDocument.body.getAttribute("data-observe-type"),
                    MutationObserver === topMutationObserver,
                    setTimeout === topSetTimeout
                ].join("|");
            "##,
            "iframe Window platform-global replacement",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "windowPlatformGlobalNavigationResult"),
            "true|true|function|true|true"
        );
    }

    #[test]
    fn iframe_navigation_replaces_intrinsics_and_platform_prototypes() {
        // ECMA-262 §9.3 gives every Realm its own intrinsics, and HTML §7.5.1
        // creates a new Window Realm for a replacement Document. Zone.js is a
        // useful conformance shape: it patches both an ECMAScript intrinsic
        // and EventTarget.prototype with closures that resolve its Window
        // global. A later application must observe neither patch.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                frame.srcdoc = '<body><script>' +
                    'window.Zone = { marker: "first-zone" };' +
                    'Array.prototype.zoneArrayPatch = Zone.marker;' +
                    'EventTarget.prototype.zoneProbe = function () { return Zone.marker; };' +
                    'document.body.setAttribute("data-probe", new EventTarget().zoneProbe());' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();
                const firstWindow = frame.contentWindow;
                const firstEventTarget = firstWindow.EventTarget;
                const firstArrayPrototype = firstWindow.Array.prototype;
                const firstProbe = frame.contentDocument.body.getAttribute("data-probe");

                frame.srcdoc = '<body><script>' +
                    'document.body.setAttribute("data-isolation", [' +
                    'typeof Zone, ' +
                    'String("zoneArrayPatch" in Array.prototype), ' +
                    'String("zoneProbe" in EventTarget.prototype)' +
                    '].join("|"));' +
                    'console.error("child-realm-log");' +
                    '<\/script></body>';
                __trust.hydrateFrames();
                const secondWindow = frame.contentWindow;
                globalThis.windowRealmIntrinsicNavigationResult = [
                    firstProbe,
                    secondWindow !== firstWindow,
                    secondWindow.EventTarget !== firstEventTarget,
                    secondWindow.Array.prototype !== firstArrayPrototype,
                    frame.contentDocument.body.getAttribute("data-isolation"),
                    typeof Zone,
                    String("zoneArrayPatch" in Array.prototype),
                    String("zoneProbe" in EventTarget.prototype)
                ].join("|");
            "##,
            "iframe Realm intrinsic replacement",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "windowRealmIntrinsicNavigationResult"),
            "first-zone|true|true|true|undefined|false|false|undefined|false|false"
        );
        assert_eq!(
            string_value(&mut engine, "__trust.takeLogs()"),
            "error: child-realm-log"
        );
    }

    #[test]
    fn iframe_windows_do_not_share_author_global_properties_across_navigation() {
        // HTML §7.5.1 creates a new Realm and Window for an ordinary
        // cross-document navigation. Author-created Window properties belong
        // to that Window: they are neither parent globals nor properties of
        // the replacement Window behind the stable WindowProxy.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");

                frame.srcdoc = '<body><script>' +
                    'window.applicationOwnedGlobal = { documentName: "first" };' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();
                const firstValue = frame.contentWindow.applicationOwnedGlobal.documentName;
                const leakedToParent = "applicationOwnedGlobal" in window;

                frame.srcdoc = '<body><script>' +
                    'document.body.setAttribute("data-inherited", ' +
                    'String("applicationOwnedGlobal" in window));' +
                    'window.applicationOwnedGlobal = { documentName: "second" };' +
                    '<\/script></body>';
                __trust.hydrateFrames();
                globalThis.windowAuthorGlobalNavigationResult = [
                    firstValue,
                    leakedToParent,
                    frame.contentDocument.body.getAttribute("data-inherited"),
                    frame.contentWindow.applicationOwnedGlobal.documentName,
                    "applicationOwnedGlobal" in window
                ].join("|");
            "##,
            "iframe Window author-global replacement",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "windowAuthorGlobalNavigationResult"),
            "first|false|false|second|false"
        );
    }

    #[test]
    fn iframe_cross_document_navigation_discards_old_animation_frame_callbacks() {
        // HTML §8.12 stores each Window's animation-frame callback map on
        // that Window's associated Document. Replacing the active Window and
        // Document must make callbacks queued by the old application
        // unreachable; they cannot run against the replacement Document.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                frame.srcdoc = '<body><script>' +
                    'requestAnimationFrame(function () {' +
                    'document.body.setAttribute("data-old-frame", "yes"); });' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();

                frame.srcdoc = '<body><script>' +
                    'requestAnimationFrame(function () {' +
                    'document.body.setAttribute("data-new-frame", "yes"); });' +
                    '<\/script></body>';
                __trust.hydrateFrames();
                globalThis.animationFrameNavigation = frame;
                __trust.tickTo(__trust.now() + 20);
            "##,
            "iframe animation-frame navigation isolation",
        )
        .unwrap();

        assert_eq!(
            string_value(
                &mut engine,
                "[animationFrameNavigation.contentDocument.body.getAttribute('data-old-frame'), animationFrameNavigation.contentDocument.body.getAttribute('data-new-frame')].join('|')"
            ),
            "|yes"
        );
    }

    #[test]
    fn iframe_navigation_during_animation_frame_does_not_run_replacement_callback_early() {
        // HTML §8.12 snapshots the keys of each target Document's callback
        // map. Handles are only unique within that map: replacing an iframe's
        // Document during another target's callback can reuse handle 1, but the
        // replacement callback was not in the old map snapshot and must wait
        // for the next rendering opportunity.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");

                requestAnimationFrame(function () {
                    frame.srcdoc = '<body><script>' +
                        'requestAnimationFrame(function () {' +
                        'document.body.setAttribute("data-replacement-frame", "yes"); });' +
                        '<\/script></body>';
                    __trust.hydrateFrames();
                });

                frame.srcdoc = '<body><script>' +
                    'requestAnimationFrame(function () {' +
                    'document.body.setAttribute("data-stale-frame", "yes"); });' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();
                globalThis.animationFrameCollisionNavigation = frame;

                __trust.tickTo(__trust.now() + 20);
                globalThis.replacementFrameAfterFirstOpportunity =
                    frame.contentDocument.body.getAttribute("data-replacement-frame");
                __trust.tickTo(__trust.now() + 20);
            "##,
            "iframe animation-frame handle collision during navigation",
        )
        .unwrap();

        assert_eq!(
            string_value(
                &mut engine,
                "[replacementFrameAfterFirstOpportunity, animationFrameCollisionNavigation.contentDocument.body.getAttribute('data-stale-frame'), animationFrameCollisionNavigation.contentDocument.body.getAttribute('data-replacement-frame')].join('|')"
            ),
            "||yes"
        );
    }

    #[test]
    fn iframe_cross_document_navigation_discards_old_timers() {
        // HTML §8.7 gives every WindowOrWorkerGlobalScope its own initially
        // empty timer-ID map. A replacement Window therefore neither runs the
        // old Document's timeout/interval tasks nor continues its ID sequence.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
                const html = document.createElement("html");
                const body = document.createElement("body");
                document.appendChild(html); html.appendChild(body);
                const frame = document.createElement("iframe");
                frame.srcdoc = '<body><script>' +
                    'setTimeout(function () {' +
                    'document.body.setAttribute("data-old-timeout", "yes"); }, 10);' +
                    'setInterval(function () {' +
                    'document.body.setAttribute("data-old-interval", "yes"); }, 10);' +
                    '<\/script></body>';
                body.appendChild(frame);
                __trust.hydrateFrames();

                frame.srcdoc = '<body><script>' +
                    'const id = setTimeout(function () {' +
                    'document.body.setAttribute("data-new-timeout", "yes"); }, 10);' +
                    'document.body.setAttribute("data-new-id", id);' +
                    '<\/script></body>';
                __trust.hydrateFrames();
                globalThis.timerNavigation = frame;
                const deadline = __trust.now() + 20;
                for (let i = 0; i < 4; i++) __trust.tickTo(deadline);
            "##,
            "iframe timer navigation isolation",
        )
        .unwrap();

        assert_eq!(
            string_value(
                &mut engine,
                "[timerNavigation.contentDocument.body.getAttribute('data-old-timeout'), timerNavigation.contentDocument.body.getAttribute('data-old-interval'), timerNavigation.contentDocument.body.getAttribute('data-new-timeout'), timerNavigation.contentDocument.body.getAttribute('data-new-id')].join('|')"
            ),
            "||yes|1"
        );
    }

    #[test]
    fn iframe_javascript_urls_execute_and_can_replace_the_frame_document() {
        // HTML §7.4.2.3.2: javascript: navigations run decoded classic script
        // in the target navigable; only a string completion creates replacement
        // HTML. A script completion that mutates location is handled by the
        // frame's ordinary queued src navigation path.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);

            const mutated = document.createElement("iframe");
            mutated.setAttribute("src", "javascript:document.body.innerHTML = '<p id=mutated>done</p>'");
            body.appendChild(mutated);

            const replaced = document.createElement("iframe");
            replaced.setAttribute("src", "javascript:'<p id=replaced>done</p>'");
            body.appendChild(replaced);

            __trust.hydrateFrames();
            globalThis.javascriptFrameResult = [
                mutated.contentDocument.body.querySelector("#mutated").textContent,
                replaced.contentDocument.body.querySelector("#replaced").textContent
            ].join("|");
            "##,
            "iframe javascript URL navigation",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "javascriptFrameResult"),
            "done|done"
        );
    }

    #[test]
    fn iframe_parser_document_write_stays_inside_its_source_parent() {
        // WHATWG HTML §8.4.3 applies the active parser's insertion point to a
        // nested navigable as well. SCM Player relies on this exact pattern:
        // its parser script writes the full-size content iframe inside an
        // absolutely positioned #contentW container.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            const frame = document.createElement("iframe");
            frame.srcdoc = '<div id="contentW"><script>' +
                'document.write(\'<iframe id="content"></iframe>\')' +
                '<\/script><span id="after">after</span></div>' +
                '<div id="playerW">player</div>';
            body.appendChild(frame);
            __trust.hydrateFrames();
            const child = frame.contentDocument;
            const content = child.getElementById("content");
            globalThis.frameWriteResult = [
                content.parentNode.id,
                content.nextElementSibling.id,
                child.getElementById("playerW").previousElementSibling.id,
                child.currentScript === null
            ].join("|");
            "##,
            "iframe parser document.write insertion point",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "frameWriteResult"),
            "contentW|after|contentW|true"
        );
    }

    #[test]
    fn iframe_documents_expose_and_dispatch_global_event_handler_idl_attributes() {
        // WHATWG HTML §8.1.8.2: GlobalEventHandlers is included by
        // HTMLElement, Document, and Window. A nested Document must expose the
        // same oninput IDL attribute as the top-level Document; React uses this
        // exact standards probe to select the modern input-event path.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            const frame = document.createElement("iframe");
            frame.srcdoc = '<input id="field"><script>' +
                'let documentHits = 0, windowHits = 0;' +
                'document.oninput = function () { documentHits++; };' +
                'window.oninput = function () { windowHits++; };' +
                'let hashHits = 0;' +
                'window.onhashchange = function childHashHandler() {' +
                'document.body.setAttribute("data-hash-handler", String(++hashHits));' +
                '};' +
                'window.callHashHandlerAfterNavigation = function () {' +
                'location.hash = "#child-route";' +
                'const handler = window.onhashchange;' +
                'document.body.setAttribute("data-hash-type", typeof handler);' +
                'handler();' +
                '};' +
                'document.dispatchEvent(new Event("input", { bubbles: true }));' +
                'document.body.setAttribute("data-result", [' +
                '"oninput" in document, "oninput" in window,' +
                '"oninput" in document.getElementById("field"),' +
                'documentHits, windowHits].join("|"));' +
                '<\/script>';
            body.appendChild(frame);
            __trust.hydrateFrames();
            const childHashHandler = frame.contentWindow.onhashchange;
            if (typeof childHashHandler === "function") childHashHandler();
            frame.contentWindow.callHashHandlerAfterNavigation();
            globalThis.frameEventHandlerResult =
                [frame.contentDocument.body.getAttribute("data-result"),
                 typeof childHashHandler,
                 childHashHandler && childHashHandler.name,
                 frame.contentDocument.body.getAttribute("data-hash-handler"),
                 frame.contentDocument.body.getAttribute("data-hash-type")].join("|");
            "##,
            "iframe GlobalEventHandlers",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "frameEventHandlerResult"),
            "true|true|true|1|1|function|childHashHandler|3|function"
        );
    }

    #[test]
    fn iframe_element_events_use_the_child_document_and_window_path() {
        // DOM §2.9 dispatches through the target's own tree. HTML gives a
        // Document its associated Window as event parent, while an iframe's
        // content Document is never a descendant of the embedding element.
        // React delegates clicks to the child Document, so this boundary is
        // observable even when the iframe nodes share TRust's paint arena.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            globalThis.topDocumentClickHits = 0;
            globalThis.topWindowClickHits = 0;
            document.addEventListener("click", function () { topDocumentClickHits++; });
            window.addEventListener("click", function () { topWindowClickHits++; });
            const frame = document.createElement("iframe");
            frame.srcdoc = '<button id="target">go</button><script>' +
                'let targetHits = 0, childDocumentHits = 0, childWindowHits = 0;' +
                'document.getElementById("target").addEventListener("click", function () { targetHits++; });' +
                'document.addEventListener("click", function () { childDocumentHits++; });' +
                'window.addEventListener("click", function () { childWindowHits++; });' +
                'document.getElementById("target").click();' +
                'document.body.setAttribute("data-result", [' +
                'targetHits, childDocumentHits, childWindowHits].join("|"));' +
                '<\/script>';
            body.appendChild(frame);
            __trust.hydrateFrames();
            globalThis.frameElementEventPathResult = [
                frame.contentDocument.body.getAttribute("data-result"),
                topDocumentClickHits, topWindowClickHits
            ].join("|");
            "##,
            "iframe element event path",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "frameElementEventPathResult"),
            "1|1|1|0|0"
        );
    }

    #[test]
    fn iframe_classic_scripts_share_their_global_lexical_environment() {
        // HTML §8.1.4.4 runs a classic script through ECMA-262
        // ScriptEvaluation. Each Script Record uses its Realm's [[GlobalEnv]]
        // for both environments, so a function from an earlier script can
        // resolve a top-level lexical declared by a later script. Speedometer's
        // Perf Dashboard does this when mockAPIs() reads `const RemoteAPI`
        // from its subsequently loaded bundle.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            const frame = document.createElement("iframe");
            frame.srcdoc = '<body><script>' +
                'function readLaterFrameLexical() {' +
                'document.body.setAttribute("data-result", LaterFrameLexical.value);' +
                '}' +
                '<\/script><script>' +
                'const LaterFrameLexical = { value: "visible" };' +
                'readLaterFrameLexical();' +
                '<\/script></body>';
            body.appendChild(frame);
            __trust.hydrateFrames();
            globalThis.frameGlobalLexicalResult =
                frame.contentDocument.body.getAttribute("data-result");
            "##,
            "iframe classic ScriptEvaluation",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "String(frameGlobalLexicalResult)"),
            "visible"
        );
    }

    #[test]
    fn iframe_animation_frame_override_is_scoped_to_its_window() {
        // HTML §8.12 associates AnimationFrameProvider state with a target
        // object, and HTML §7.2 gives every browsing context its own Window
        // behind a stable WindowProxy. A child may replace its writable rAF
        // method without replacing the parent's method; later calls through
        // contentWindow must still enter the child Window.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            const topRequestAnimationFrame = requestAnimationFrame;
            const frame = document.createElement("iframe");
            frame.srcdoc = '<body><script>' +
                'requestAnimationFrame = function (callback) {' +
                'callback(123); return 77; };' +
                'function callChildRAF() {' +
                'return requestAnimationFrame(function (timestamp) {' +
                'document.body.setAttribute("data-timestamp", timestamp); });' +
                '}' +
                '<\/script></body>';
            body.appendChild(frame);
            __trust.hydrateFrames();
            const parentPreservedBefore = requestAnimationFrame === topRequestAnimationFrame;
            const childHandle = frame.contentWindow.callChildRAF();
            globalThis.frameAnimationOverrideResult = [
                parentPreservedBefore,
                requestAnimationFrame === topRequestAnimationFrame,
                childHandle,
                frame.contentDocument.body.getAttribute("data-timestamp")
            ].join("|");
            "##,
            "iframe AnimationFrameProvider isolation",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "frameAnimationOverrideResult"),
            "true|true|77|123"
        );
    }

    #[test]
    fn iframe_documents_create_filtered_tree_walkers_and_node_iterators() {
        // DOM §4.5 and §6: every Document creates traversal objects retaining
        // the supplied root, whatToShow mask, and filter. Lit creates a
        // TreeWalker from its benchmark iframe's Document while stamping
        // template parts, so the nested-document surface is observable.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            const frame = document.createElement("iframe");
            frame.srcdoc = '<main><section><span>A</span><!--marker--><span>B</span></section></main>' +
                '<scr' + 'ipt>' +
                'const root = document.body;' +
                'const filter = { acceptNode(node) {' +
                'return node.localName === "span" ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP; }};' +
                'const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, filter);' +
                'const walked = []; let node; while ((node = walker.nextNode())) walked.push(node.textContent);' +
                'const iterator = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT,' +
                'node => node.localName === "span" ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_REJECT);' +
                'const iterated = []; while ((node = iterator.nextNode())) iterated.push(node.textContent);' +
                'const previous = iterator.previousNode();' +
                'root.setAttribute("data-result", [' +
                'walker.root === root, walker.whatToShow === NodeFilter.SHOW_ELEMENT,' +
                'walker.filter === filter, walked.join(","), iterated.join(","),' +
                'previous && previous.textContent, iterator.pointerBeforeReferenceNode].join("|"));' +
                '</scr' + 'ipt>';
            body.appendChild(frame);
            __trust.hydrateFrames();
            globalThis.frameTraversalResult =
                frame.contentDocument.body.getAttribute("data-result");
            "##,
            "iframe Document traversal",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "frameTraversalResult"),
            "true|true|true|A,B|A,B|B|true"
        );
    }

    #[test]
    fn document_point_queries_follow_paint_order_and_nested_document_scope() {
        // CSSOM View §5: hit-test boxes in topmost-first paint order, exclude
        // boxes that are not pointer targets, keep iframe Documents scoped to
        // their own viewport, and append the relevant root element last.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const head = document.createElement("head");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(head); html.appendChild(body);
            const style = document.createElement("style");
            style.textContent = `
                html, body { margin: 0; padding: 0; }
                #under, #over, #ignored { position: absolute; left: 10px; top: 10px; width: 80px; height: 60px; }
                #under { z-index: 1; }
                #over { z-index: 2; }
                #ignored { z-index: 3; pointer-events: none; }
                iframe { position: absolute; left: 120px; top: 10px; width: 160px; height: 90px; border: 0; }
            `;
            head.appendChild(style);
            for (const id of ["under", "over", "ignored"]) {
                const node = document.createElement("div"); node.id = id; body.appendChild(node);
            }
            const frame = document.createElement("iframe");
            frame.srcdoc = '<style>html,body{margin:0;width:100%;height:100%}#child{width:100%;height:100%}</style><div id="child"></div>';
            body.appendChild(frame);
            __trust.hydrateFrames();

            const top = document.elementsFromPoint(20, 20);
            const frameRect = frame.getBoundingClientRect();
            const parentAtFrame = document.elementFromPoint(frameRect.left + 10, frameRect.top + 10);
            const childDoc = frame.contentDocument;
            const child = childDoc.elementsFromPoint(10, 10);
            let missingThrows = false, infinityThrows = false;
            try { document.elementFromPoint(1); } catch (error) { missingThrows = error instanceof TypeError; }
            try { document.elementFromPoint(Infinity, 1); } catch (error) { infinityThrows = error instanceof TypeError; }
            globalThis.pointQueryResult = [
                top[0] && top[0].id, top.some(node => node.id === "ignored"),
                top[top.length - 1] === document.documentElement,
                parentAtFrame === frame,
                child[0] && child[0].id,
                child[child.length - 1] === childDoc.documentElement,
                document.elementFromPoint(-1, 0) === null,
                document.elementsFromPoint(0, -1).length,
                missingThrows, infinityThrows
            ].join("|");
            "##,
            "CSSOM View point queries",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "pointQueryResult"),
            "over|false|true|true|child|true|true|0|true|true"
        );
    }

    #[test]
    fn offset_parent_stays_within_its_document_and_follows_containing_blocks() {
        // CSSOM View §7 and CSS Positioned Layout §2.1: offsetParent walks the
        // element's own flat tree, returns null for roots/body/fixed elements
        // without a fixed containing block, and selects the nearest ancestor
        // establishing the applicable positioning containing block.  In
        // particular, an iframe Document must not cross into its owner page or
        // cycle back into its body during CodeMirror's clipping-ancestor walk.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            const frame = document.createElement("iframe");
            frame.srcdoc = '<style>html,body{margin:0}#positioned{position:relative;border:3px solid black}' +
                '#absolute{position:absolute;left:7px;top:9px}#fixed{position:fixed}' +
                '#hidden{display:none}</style><main id="static"><div id="positioned">' +
                '<span id="absolute">absolute</span><span id="fixed">fixed</span>' +
                '<span id="hidden">hidden</span></div></main>';
            body.appendChild(frame);
            __trust.hydrateFrames();

            const childDocument = frame.contentDocument;
            const staticNode = childDocument.getElementById("static");
            const positioned = childDocument.getElementById("positioned");
            const absolute = childDocument.getElementById("absolute");
            const fixed = childDocument.getElementById("fixed");
            const hidden = childDocument.getElementById("hidden");
            let ancestor = absolute, steps = 0;
            while (ancestor && ancestor !== childDocument.body && steps++ < 12) {
                const style = getComputedStyle(ancestor);
                ancestor = style.position === "absolute" || style.position === "fixed"
                    ? ancestor.offsetParent : ancestor.parentNode;
            }
            globalThis.offsetParentResult = [
                absolute.ownerDocument === childDocument,
                childDocument.documentElement.ownerDocument === childDocument,
                frame.ownerDocument === document,
                staticNode.offsetParent === childDocument.body,
                absolute.offsetParent === positioned,
                fixed.offsetParent === null,
                hidden.offsetParent === null,
                childDocument.body.offsetParent === null,
                childDocument.documentElement.offsetParent === null,
                ancestor === childDocument.body && steps < 12,
                Number.isInteger(absolute.offsetTop),
                Number.isInteger(absolute.offsetLeft),
                childDocument.body.offsetTop === 0,
                childDocument.body.offsetLeft === 0
            ].join("|");
            "##,
            "CSSOM View offset parent",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "offsetParentResult"),
            "true|true|true|true|true|true|true|true|true|true|true|true|true|true"
        );
    }

    #[test]
    fn inner_html_descendants_are_queryable_synchronously_after_insertion() {
        // HTML §13.3 appends text in script-data/raw-text parents literally
        // during fragment serialization; normal text is escaped. DOM §4.2.6
        // querySelectorAll then returns the static result of scope-matching at
        // call time. Backbone/Underscore stores item markup in precisely this
        // `<script type="text/template">` shape.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            const html = document.createElement("html");
            const body = document.createElement("body");
            const list = document.createElement("ul");
            document.appendChild(html); html.appendChild(body); body.appendChild(list);
            const source = '<div class="view"><input class="toggle"><button class="destroy"></button></div>';
            const template = document.createElement("script");
            template.setAttribute("type", "text/template");
            template.textContent = source;
            body.appendChild(template);
            for (let i = 0; i < 100; i++) {
                const item = document.createElement("li");
                item.innerHTML = template.innerHTML;
                list.appendChild(item);
            }
            const normal = document.createElement("div");
            normal.textContent = "<b>&";
            const hidden = document.createElement("aside");
            hidden.hidden = true; hidden.textContent = "kept"; body.appendChild(hidden);
            const host = document.createElement("x-serialization-host");
            host.innerHTML = '<i class="light">light</i>';
            host.attachShadow({ mode: "open" }).innerHTML = '<b class="shadow">shadow</b>';
            body.appendChild(host);
            globalThis.synchronousQueryResult = [
                template.innerHTML === source,
                template.outerHTML.includes(source),
                normal.innerHTML === "&lt;b&gt;&amp;",
                hidden.outerHTML.includes("kept"),
                host.outerHTML.includes('class="light"') && !host.outerHTML.includes('class="shadow"'),
                document.querySelectorAll("li").length,
                document.querySelectorAll(".toggle").length,
                document.querySelectorAll(".destroy").length
            ].join("|");
            "#,
            "synchronous innerHTML query",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "synchronousQueryResult"),
            "true|true|true|true|true|100|100|100"
        );
    }

    #[test]
    fn text_decoder_supports_utf16_labels_boms_streaming_and_fatal_errors() {
        // Encoding §4.2/§7.2 and §14.2: UTF-16 labels, BOM precedence, and
        // replacement versus fatal handling are part of the browser API. The
        // one-byte streaming case checks that a decoder carries an incomplete
        // code unit into the next decode call.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            const leBytes = new Uint8Array([0xff, 0xfe, 0x48, 0x00, 0x3d, 0xd8, 0x00, 0xde]);
            const beBytes = new Uint8Array([0xfe, 0xff, 0x00, 0x48, 0xd8, 0x3d, 0xde, 0x00]);
            const le = new TextDecoder('utf-16', { fatal: false });
            const be = new TextDecoder('utf-16be');
            const keptBom = new TextDecoder('utf-16le', { ignoreBOM: true });
            const streamed = new TextDecoder('utf-16le');
            let fatal = false;
            try { new TextDecoder('utf-16le', { fatal: true }).decode(new Uint8Array([0x48])); }
            catch (error) { fatal = error instanceof TypeError; }
            globalThis.textDecoderResult = [
                le.encoding, le.decode(leBytes).codePointAt(1) === 0x1f600,
                be.encoding, be.decode(beBytes) === 'H😀',
                keptBom.decode(leBytes).charCodeAt(0) === 0xfeff,
                streamed.decode(new Uint8Array([0x48]), { stream: true }) === '',
                streamed.decode(new Uint8Array([0])) === 'H',
                le.decode(new Uint8Array([0x48])) === '�', fatal
            ].join('|');
            "#,
            "TextDecoder UTF-16",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "textDecoderResult"),
            "utf-16le|true|utf-16be|true|true|true|true|true|true"
        );
    }

    #[test]
    fn text_encoder_encode_into_observes_utf8_boundaries_and_surrogates() {
        // Encoding §7.4: encodeInto reports UTF-16 code units read and never emits a partial
        // scalar value when the destination is too small.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            const target = new Uint8Array(5);
            const partial = new TextEncoder().encodeInto('Aé😀', target);
            const lone = new Uint8Array(3);
            const replacement = new TextEncoder().encodeInto('\ud800', lone);
            globalThis.textEncoderResult = [
                partial.read, partial.written, Array.from(target).slice(0, partial.written).join(','),
                replacement.read, replacement.written, Array.from(lone).slice(0, replacement.written).join(',')
            ].join('|');
            "#,
            "TextEncoder encodeInto",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "textEncoderResult"),
            "2|3|65,195,169|1|3|239,191,189"
        );
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
    fn intersection_observer_records_then_notifies_on_its_task_source() {
        // Intersection Observer §§3.2.4–3.2.6: the rendering update queues
        // entries and one IntersectionObserver task; it does not synchronously
        // invoke callbacks. `takeRecords()` drains queued entries before that
        // task, and repeated geometry updates with no threshold crossing do not
        // duplicate a record.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            globalThis.ioResult = { callbacks: 0, entries: 0 };
            const target = document.createElement('div');
            const observer = new IntersectionObserver(function (entries) {
                ioResult.callbacks++;
                ioResult.entries += entries.length;
            });
            observer.observe(target);
            ioResult.firstQueued = __trust.updateIntersections();
            ioResult.callbackWasSync = ioResult.callbacks !== 0;
            ioResult.hadTask = __trust.hasPlatformTask();
            ioResult.taken = observer.takeRecords().length;

            observer.unobserve(target);
            observer.observe(target);
            ioResult.secondQueued = __trust.updateIntersections();
            ioResult.duplicateQueued = __trust.updateIntersections();
            ioResult.ranTask = __trust.runPlatformTask();
            "#,
            "IntersectionObserver task source",
        )
        .unwrap();

        assert_eq!(
            string_value(
                &mut engine,
                "[ioResult.firstQueued, ioResult.callbackWasSync, ioResult.hadTask, ioResult.taken, ioResult.secondQueued, ioResult.duplicateQueued, ioResult.ranTask, ioResult.callbacks, ioResult.entries].join(',')"
            ),
            "1,false,true,1,1,0,true,1,1"
        );
    }

    #[test]
    fn observer_initial_updates_request_rendering_without_timer_tasks() {
        // Resize Observer §3.4 and Intersection Observer §3.2.4 integrate with
        // HTML's update-the-rendering algorithm. Registering even a large set
        // requests one host rendering opportunity and queues no timer tasks.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            const io = new IntersectionObserver(function () {});
            const ro = new ResizeObserver(function () {});
            for (let i = 0; i < 128; i++) {
                const target = document.createElement('div');
                io.observe(target);
                ro.observe(target);
            }
            globalThis.observerQueuesBefore = __trust.taskQueueState();
            globalThis.observerRenderingBefore = __trust.hasRenderingUpdate();
            __trust.updateResizes();
            __trust.updateIntersections();
            globalThis.observerQueuesAfter = __trust.taskQueueState();
            globalThis.observerRenderingAfter = __trust.hasRenderingUpdate();
            "#,
            "observer initial update coalescing",
        )
        .unwrap();

        let before = string_value(&mut engine, "observerQueuesBefore");
        assert!(before.contains("timers=0(once=0,interval=0)"), "{before}");
        assert_eq!(
            string_value(&mut engine, "String(observerRenderingBefore)"),
            "true"
        );
        let after = string_value(&mut engine, "observerQueuesAfter");
        assert!(after.contains("timers=0(once=0,interval=0)"), "{after}");
        assert!(after.contains("intersection=1"), "{after}");
        assert_eq!(
            string_value(&mut engine, "String(observerRenderingAfter)"),
            "false"
        );
    }

    #[test]
    fn detached_node_wrapper_cache_does_not_root_unreachable_wrappers() {
        // Web IDL wrapper identity applies while a platform object remains
        // observable. ECMA-262 WeakRef preserves same-job identity without a
        // strong cache edge, and native FinalizationRegistry cleanup removes
        // dead id entries after collection. A DOM churn workload must not
        // retain every transient wrapper for the lifetime of the page.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            globalThis.keptWrapper = document.createElement("div");
            (function () {
                for (let i = 0; i < 2048; i++) document.createElement("span");
            })();
            globalThis.detachedQueryRoot = document.createElement("div");
            detachedQueryRoot.innerHTML = "<i></i>".repeat(2048);
            (function () {
                detachedQueryRoot.querySelectorAll("i");
            })();
            globalThis.wrapperCacheBefore = __trust.nodeWrapperCacheState().join(",");
            "#,
            "detached wrapper churn",
        )
        .unwrap();
        run_microtask_checkpoint(&mut engine);

        for _ in 0..3 {
            engine.collect_garbage_at_idle();
            run_microtask_checkpoint(&mut engine);
        }
        let before = string_value(&mut engine, "wrapperCacheBefore")
            .split_once(',')
            .unwrap()
            .0
            .parse::<usize>()
            .unwrap();
        let after = string_value(&mut engine, "__trust.nodeWrapperCacheState().join(',')")
            .split_once(',')
            .unwrap()
            .0
            .parse::<usize>()
            .unwrap();

        assert!(before >= 2048, "cache did not observe the churn: {before}");
        assert!(
            after < before / 4,
            "unreachable wrappers remained rooted: before={before}, after={after}"
        );
        assert_eq!(
            string_value(
                &mut engine,
                "String(keptWrapper === keptWrapper && document === document)"
            ),
            "true"
        );
    }

    #[test]
    fn inner_html_replace_all_preserves_detached_wrapper_identity_and_listeners() {
        // HTML "innerHTML" runs DOM "replace all". Removing a subtree changes
        // connectedness, but it does not discard its platform-object identity,
        // event listeners, light-tree relationships, or attached shadow tree.
        // Keep this observable contract pinned while the binding batches its
        // internal wrapper-retention bookkeeping for bulk replacements.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            const replaceAllMount = document.createElement("main");
            document.appendChild(replaceAllMount);
            replaceAllMount.innerHTML = "<section><button>old</button><x-host></x-host></section>";
            const replaceAllOldRoot = replaceAllMount.firstChild;
            const replaceAllOldButton = replaceAllOldRoot.querySelector("button");
            const replaceAllShadowHost = replaceAllOldRoot.querySelector("x-host");
            const replaceAllShadow = replaceAllShadowHost.attachShadow({ mode: "open" });
            replaceAllShadow.innerHTML = "<button>shadow</button>";
            const replaceAllShadowButton = replaceAllShadow.firstChild;
            let replaceAllLightClicks = 0;
            let replaceAllShadowClicks = 0;
            replaceAllOldButton.addEventListener("click", () => replaceAllLightClicks++);
            replaceAllShadowButton.addEventListener("click", () => replaceAllShadowClicks++);

            replaceAllMount.innerHTML = "<p>new</p>";
            replaceAllOldButton.dispatchEvent(new Event("click"));
            replaceAllShadowButton.dispatchEvent(new Event("click"));
            const replaceAllDetachedResult = [
                replaceAllOldRoot.isConnected,
                replaceAllOldRoot.firstChild === replaceAllOldButton,
                replaceAllShadowHost.shadowRoot === replaceAllShadow,
                replaceAllShadow.firstChild === replaceAllShadowButton,
                replaceAllLightClicks,
                replaceAllShadowClicks
            ].join("|");

            replaceAllMount.appendChild(replaceAllOldRoot);
            const replaceAllReinsertedResult = [
                replaceAllOldRoot.isConnected,
                replaceAllMount.lastChild === replaceAllOldRoot,
                replaceAllOldRoot.querySelector("button") === replaceAllOldButton,
                replaceAllShadowHost.shadowRoot.firstChild === replaceAllShadowButton
            ].join("|");

            const replaceAllDetachedMount = document.createElement("div");
            replaceAllDetachedMount.innerHTML = "<button>detached</button>";
            const replaceAllAlwaysDetachedButton = replaceAllDetachedMount.firstChild;
            let replaceAllDetachedClicks = 0;
            replaceAllAlwaysDetachedButton.addEventListener("click", () => replaceAllDetachedClicks++);
            replaceAllDetachedMount.innerHTML = "";
            replaceAllAlwaysDetachedButton.dispatchEvent(new Event("click"));
            "#,
            "innerHTML replace-all wrapper retention",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "replaceAllDetachedResult"),
            "false|true|true|true|1|1"
        );
        assert_eq!(
            string_value(&mut engine, "replaceAllReinsertedResult"),
            "true|true|true|true"
        );
        assert_eq!(
            string_value(&mut engine, "String(replaceAllDetachedClicks)"),
            "1"
        );
    }

    #[test]
    fn connected_custom_element_wrapper_retains_identity_and_shadow_state_across_gc() {
        // Web IDL interface conversion returns the JavaScript object
        // representing the same platform object. DOM §4.2.2 also makes a
        // shadow root persistently attached to its host. A connected native
        // node therefore has to keep the wrapper carrying our custom-element
        // state alive even when page code temporarily holds no strong handle.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            customElements.define("x-retained-state", class extends HTMLElement {
                constructor() {
                    super();
                    this.answer = 42;
                    this.attachShadow({ mode: "open" }).innerHTML = "<b>kept</b>";
                }
            });
            const wrapperRetentionMount = document.createElement("main");
            document.appendChild(wrapperRetentionMount);
            wrapperRetentionMount.innerHTML = "<span>selected</span>";
            (function () {
                const selected = wrapperRetentionMount.querySelectorAll("span")[0];
                selected.selectorState = 17;
                globalThis.connectedSelectorWeak = new WeakRef(selected);
            })();
            (function () {
                const element = document.createElement("x-retained-state");
                wrapperRetentionMount.appendChild(element);
                globalThis.connectedWrapperWeak = new WeakRef(element);
            })();
            "#,
            "connected wrapper retention setup",
        )
        .unwrap();
        run_microtask_checkpoint(&mut engine);

        for _ in 0..3 {
            engine.collect_garbage_at_idle();
            run_microtask_checkpoint(&mut engine);
        }

        assert_eq!(
            string_value(
                &mut engine,
                r#"(function () {
                    const element = document.querySelector("x-retained-state");
                    return [
                        element === connectedWrapperWeak.deref(),
                        element.answer,
                        element.shadowRoot && element.shadowRoot.textContent,
                        wrapperRetentionMount.querySelectorAll("span")[0]
                            === connectedSelectorWeak.deref(),
                        wrapperRetentionMount.querySelectorAll("span")[0].selectorState
                    ].join("|");
                })()"#,
            ),
            "true|42|kept|true|17"
        );
    }

    #[test]
    fn html_timer_initialization_applies_the_nested_four_millisecond_clamp() {
        // HTML §8.7 "timer initialization steps" is shared by Window and
        // workers. Levels 1–6 preserve a requested zero delay; a timer created
        // from level 6 or later is clamped to 4 ms. Window callbacks receive
        // the Window global as their callback `this` value.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            globalThis.timerResult = { nesting: [], waits: [], thisValues: [] };
            function chain() {
                timerResult.thisValues.push(this === window);
                if (timerResult.nesting.length < 7) {
                    setTimeout(chain, 0);
                    const info = __trust.nextTimerInfo();
                    timerResult.nesting.push(info.nesting);
                    timerResult.waits.push(info.wait);
                }
            }
            setTimeout(chain, Infinity);
            timerResult.initialWait = __trust.nextTimerInfo().wait;
            "#,
            "timer nesting setup",
        )
        .unwrap();
        for _ in 0..8 {
            let deadline = eval_value(&mut engine, "__trust.nextDeadline()", "timer deadline")
                .unwrap()
                .as_num_opt()
                .unwrap();
            let dispatched = dispatch_timer_task_to(&mut engine, deadline);
            let ran = match dispatched {
                Ok(ran) => ran,
                Err(error) => panic!("{}", describe_eval_error(&mut engine, error, "timer task")),
            };
            assert!(ran);
            run_microtask_checkpoint(&mut engine);
        }
        assert_eq!(
            string_value(&mut engine, "String(timerResult.initialWait)"),
            "0"
        );
        assert_eq!(
            string_value(&mut engine, "timerResult.nesting.join(',')"),
            "2,3,4,5,6,7,8"
        );
        assert_eq!(
            string_value(&mut engine, "timerResult.waits.join(',')"),
            "0,0,0,0,0,4,4"
        );
        assert_eq!(
            string_value(&mut engine, "String(timerResult.thisValues.every(Boolean))"),
            "true"
        );
    }

    #[test]
    fn top_level_timer_dispatch_preserves_author_recursion_headroom() {
        // HTML §8.7 invokes the Function handler itself with WindowProxy as
        // callback-this. A top-level task needs no nested-navigable state
        // switch; routing it through two extra JS callbacks made otherwise
        // valid author recursion hit Lumen's bounded call-stack guard early.
        // Rust's test harness provisions a much smaller native stack than the 64 MiB resident
        // page thread. Exercise meaningful author depth on an explicitly sized host stack, as
        // Lumen's configurable recursion-budget test does, so the native guard page cannot win
        // before the JavaScript assertion on otherwise loaded test runs.
        std::thread::Builder::new()
            .name(String::from("trust-timer-depth-test"))
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let mut engine = platform_engine();
                eval(
                    &mut engine,
                    r#"
                    globalThis.timerDepthResult = "pending";
                    function timerDepthRecurse(n) {
                        return n ? timerDepthRecurse(n - 1) : 0;
                    }
                    setTimeout(function () {
                        try {
                            timerDepthRecurse(124);
                            timerDepthResult = this === window ? "ok" : "wrong-this";
                        } catch (error) {
                            timerDepthResult = error.name;
                        }
                    }, 0);
                    "#,
                    "timer recursion setup",
                )
                .unwrap();
                let dispatched = dispatch_timer_task_to(&mut engine, 1000.0);
                let ran = match dispatched {
                    Ok(ran) => ran,
                    Err(error) => {
                        panic!("{}", describe_eval_error(&mut engine, error, "timer task"))
                    }
                };
                assert!(ran);
                assert_eq!(string_value(&mut engine, "timerDepthResult"), "ok");
            })
            .expect("spawn timer-depth test thread")
            .join()
            .expect("timer-depth test thread");
    }

    #[test]
    fn failed_timer_diagnostic_identifies_the_scheduled_handler() {
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            setTimeout(function telegramDiagnosticFixture() {
                throw new ReferenceError("fixture failure");
            }, 0);
            "#,
            "timer diagnostic setup",
        )
        .unwrap();
        let dispatched = dispatch_timer_task_to(&mut engine, 1000.0);
        let ran = match dispatched {
            Ok(ran) => ran,
            Err(error) => panic!(
                "{}",
                describe_eval_error(&mut engine, error, "timer diagnostic task")
            ),
        };
        assert!(ran);
        let errors = string_value(&mut engine, "__trust.takeErrors()");
        assert!(errors.contains("timer: fixture failure"), "{errors}");
        assert!(
            errors.contains("Timer handler: function telegramDiagnosticFixture"),
            "{errors}"
        );
    }

    #[test]
    fn nested_frame_timer_restores_top_document_base_url() {
        // HTML §8.7 queues a timer for the WindowOrWorkerGlobalScope on which it was
        // created. Invoking a child-frame timer must not change the top Document's
        // base URL: HTML §2.4.3 derives each Document's base independently from that
        // Document's first <base href>, or from that Document's own fallback URL.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html); html.appendChild(body);
            const frame = document.createElement("iframe");
            frame.srcdoc = '<base href="https://child.example/frame/">' +
                '<script>setTimeout(function () {' +
                'topFrameTimerBase = document.baseURI;' +
                'document.body.setAttribute("data-timer-base", document.baseURI);' +
                'document.body.setAttribute("data-timer-global", String(window.topFrameTimerBase));' +
                '}, 0)<\/script>';
            body.appendChild(frame);
            __trust.hydrateFrames();
            globalThis.topBaseBeforeFrameTimer = document.baseURI;
            "#,
            "nested frame timer setup",
        )
        .unwrap();

        let dispatched = dispatch_timer_task_to(&mut engine, 1000.0);
        let ran = match dispatched {
            Ok(ran) => ran,
            Err(error) => panic!(
                "{}",
                describe_eval_error(&mut engine, error, "nested frame timer task")
            ),
        };
        assert!(ran);
        assert_eq!(
            string_value(
                &mut engine,
                "__trust.__activeFrame ? String(__trust.__activeFrame.__id) : 'top'",
            ),
            "top"
        );
        assert_eq!(
            string_value(&mut engine, "topBaseBeforeFrameTimer"),
            "https://example.com/"
        );
        assert_eq!(string_value(&mut engine, "__trust.takeErrors()"), "");
        assert_eq!(
            string_value(
                &mut engine,
                "document.querySelector('iframe').contentDocument.body.getAttribute('data-timer-base')",
            ),
            "https://child.example/frame/"
        );
        assert_eq!(
            string_value(
                &mut engine,
                "document.querySelector('iframe').contentDocument.body.getAttribute('data-timer-global')",
            ),
            "https://child.example/frame/"
        );
        assert_eq!(
            string_value(
                &mut engine,
                "document.querySelector('iframe').contentWindow.topFrameTimerBase",
            ),
            "https://child.example/frame/"
        );
        assert_eq!(
            string_value(&mut engine, "'topFrameTimerBase' in globalThis"),
            "false"
        );
        assert_eq!(
            string_value(&mut engine, "document.baseURI"),
            "https://example.com/"
        );
    }

    #[test]
    fn performance_now_retains_sub_millisecond_monotonic_resolution() {
        // High Resolution Time §§7.1 requires performance.now() to use the
        // relevant global's monotonic clock. Do not route it through Date's
        // integral millisecond time value: sufficiently fast measured work
        // would then have a browser-visible duration of zero.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            let previous = performance.now();
            let minimumPositiveDelta = Infinity;
            let monotonic = true;
            for (let i = 0; i < 2048; i++) {
                const current = performance.now();
                if (current < previous) monotonic = false;
                const delta = current - previous;
                if (delta > 0 && delta < minimumPositiveDelta)
                    minimumPositiveDelta = delta;
                previous = current;
            }
            globalThis.highResolutionClockResult =
                monotonic && minimumPositiveDelta > 0 && minimumPositiveDelta < 1;
            "#,
            "high resolution monotonic clock",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "String(highResolutionClockResult)"),
            "true"
        );
    }

    #[test]
    fn animation_frame_callbacks_share_one_rendering_opportunity() {
        // HTML §8.10 snapshots the animation-frame callback map for a rendering
        // opportunity. Cancellation from an earlier callback affects that
        // snapshot; callbacks requested while it runs wait for the next frame.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            globalThis.frameResult = { order: [], timestamps: [] };
            let third;
            requestAnimationFrame((timestamp) => {
                frameResult.order.push("first");
                frameResult.timestamps.push(timestamp);
                cancelAnimationFrame(third);
                requestAnimationFrame((nextTimestamp) => {
                    frameResult.order.push("nested");
                    frameResult.timestamps.push(nextTimestamp);
                });
            });
            requestAnimationFrame((timestamp) => {
                frameResult.order.push("second");
                frameResult.timestamps.push(timestamp);
            });
            third = requestAnimationFrame(() => frameResult.order.push("cancelled"));
            try { requestAnimationFrame(null); }
            catch (error) { frameResult.typeError = error.name; }

            frameResult.firstCount = __trust.tickTo(__trust.nextDeadline());
            frameResult.afterFirst = frameResult.order.join(",");
            frameResult.sameTimestamp = frameResult.timestamps[0] === frameResult.timestamps[1];
            frameResult.secondCount = __trust.tickTo(__trust.nextDeadline());
            frameResult.afterSecond = frameResult.order.join(",");
            frameResult.timestampAdvanced = frameResult.timestamps[2] > frameResult.timestamps[1];
            "#,
            "animation frame callback map",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "String(frameResult.firstCount)"),
            "2"
        );
        assert_eq!(
            string_value(&mut engine, "frameResult.afterFirst"),
            "first,second"
        );
        assert_eq!(
            string_value(&mut engine, "String(frameResult.sameTimestamp)"),
            "true"
        );
        assert_eq!(
            string_value(&mut engine, "String(frameResult.secondCount)"),
            "1"
        );
        assert_eq!(
            string_value(&mut engine, "frameResult.afterSecond"),
            "first,second,nested"
        );
        assert_eq!(
            string_value(&mut engine, "String(frameResult.timestampAdvanced)"),
            "true"
        );
        assert_eq!(
            string_value(&mut engine, "frameResult.typeError"),
            "TypeError"
        );
    }

    #[test]
    fn idle_callback_lists_deadlines_and_timeout_tasks_follow_the_spec() {
        // W3C requestIdleCallback §§4–5: pending callbacks become runnable only
        // when the host starts an idle period; reposted callbacks wait for the
        // next period, and an options.timeout expiry races through the idle
        // task source with didTimeout=true.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            globalThis.idleResult = { order: [] };
            const cancelled = requestIdleCallback(() => idleResult.order.push("cancelled"));
            cancelIdleCallback(cancelled);
            requestIdleCallback((deadline) => {
                idleResult.order.push("first");
                idleResult.didTimeout = deadline.didTimeout;
                idleResult.before = deadline.timeRemaining();
                const started = performance.now();
                while (performance.now() - started < 2) {}
                idleResult.after = deadline.timeRemaining();
                idleResult.tag = Object.prototype.toString.call(deadline);
                requestIdleCallback(() => idleResult.order.push("nested"));
            });
            idleResult.timerBeforePeriod = __trust.nextDeadline();
            __trust.startIdlePeriod(__trust.now() + 50);
            idleResult.firstTask = __trust.runPlatformTask();
            idleResult.afterFirst = idleResult.order.join(",");
            idleResult.samePeriodHasTask = __trust.hasPlatformTask();
            __trust.startIdlePeriod(__trust.now() + 50);
            idleResult.secondTask = __trust.runPlatformTask();

            requestIdleCallback((deadline) => {
                idleResult.order.push("timeout");
                idleResult.timeoutDidTimeout = deadline.didTimeout;
                idleResult.timeoutRemaining = deadline.timeRemaining();
            }, { timeout: 5 });
            __trust.tickTo(__trust.now() + 10);
            idleResult.timeoutQueued = __trust.hasPlatformTask();
            __trust.runPlatformTask();
            idleResult.finalOrder = idleResult.order.join(",");
            "#,
            "idle callback processing model",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "String(idleResult.timerBeforePeriod)"),
            "null"
        );
        assert_eq!(
            string_value(&mut engine, "String(idleResult.firstTask)"),
            "true"
        );
        assert_eq!(string_value(&mut engine, "idleResult.afterFirst"), "first");
        assert_eq!(
            string_value(&mut engine, "String(idleResult.samePeriodHasTask)"),
            "false"
        );
        assert_eq!(
            string_value(&mut engine, "String(idleResult.secondTask)"),
            "true"
        );
        assert_eq!(
            string_value(&mut engine, "String(idleResult.didTimeout)"),
            "false"
        );
        assert_eq!(
            string_value(&mut engine, "String(idleResult.before > idleResult.after)"),
            "true"
        );
        assert_eq!(
            string_value(&mut engine, "idleResult.tag"),
            "[object IdleDeadline]"
        );
        assert_eq!(
            string_value(&mut engine, "String(idleResult.timeoutQueued)"),
            "true"
        );
        assert_eq!(
            string_value(&mut engine, "String(idleResult.timeoutDidTimeout)"),
            "true"
        );
        assert_eq!(
            string_value(&mut engine, "String(idleResult.timeoutRemaining)"),
            "0"
        );
        assert_eq!(
            string_value(&mut engine, "idleResult.finalOrder"),
            "first,nested,timeout"
        );
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
