//! Ring-fenced Boa glue: the ONLY module allowed to import `boa_engine`.
//! If the engine ever has to change (rquickjs is the named fallback),
//! this file is the whole blast radius.
//!
//! Phase 0: engine plumbing — a budgeted context, script execution with
//! per-script error tolerance, and the canary benchmark that gates the
//! Phase 1 DOM work. Real DOM bindings replace the canary's permissive
//! stubs in Phase 1.

// Phase 0: nothing outside the tests calls this module yet; the app
// wiring (`set js on|off`, the fetch pipeline hook) lands in Phase 1.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use boa_engine::job::{Job, JobExecutor, NativeAsyncJob, PromiseJob};
use boa_engine::object::builtins::{JsArray, JsPromise};
use boa_engine::{Context, JsResult, JsString, JsValue, NativeFunction, Source};

use crate::dom::{DOCUMENT, Dom, SelectorList};

/// Wall-clock deadline for a whole page load, NETWORK-INCLUSIVE. Since
/// page fetches now run asynchronously (they no longer block the JS
/// thread), the wire no longer needs to "extend" anything — this single
/// deadline bounds compute + network together, and on expiry we render
/// whatever the DOM holds. Runaway *compute* is bounded more tightly by
/// `COMPUTE_BUDGET` and per-call by `LOOP_LIMIT`.
pub const WALL_BUDGET: Duration = Duration::from_secs(20);

/// Cumulative *execution* time a page's scripts get before we stop
/// launching more. Measures compute, not wall clock (the wire is async
/// and free), so a slow server can't starve a fast page of its scripts.
pub const COMPUTE_BUDGET: Duration = Duration::from_secs(2);

/// Most loop iterations any single script evaluation may run. Real-world
/// bundle boots measured in the canary use a few hundred thousand at
/// most; ten million leaves headroom while still catching `while(true)`
/// in well under a second.
const LOOP_LIMIT: u64 = 10_000_000;

/// A page-wide execution deadline, shared by every script on the page.
/// Page-initiated network time extends it: the wall budget meters JS,
/// not the wire.
pub struct Budget {
    deadline: std::cell::Cell<Instant>,
}

impl Budget {
    pub fn new(wall: Duration) -> Self {
        Self {
            deadline: std::cell::Cell::new(Instant::now() + wall),
        }
    }

    pub fn exhausted(&self) -> bool {
        Instant::now() >= self.deadline.get()
    }

    /// Time left until the deadline (zero if already past). Used to bound
    /// the async job loop so a hung server can't hold the page forever.
    pub fn remaining(&self) -> Duration {
        self.deadline
            .get()
            .saturating_duration_since(Instant::now())
    }

    /// A fresh window from now: per-dispatch budgets on a living page.
    pub fn rearm(&self, wall: Duration) {
        self.deadline.set(Instant::now() + wall);
    }
}

/// What a page's scripts did, for the status bar (`· JS:n!`) and
/// `app.notice`: per-script failures are collected, never fatal — a
/// broken script renders a partial page, not a blank one.
#[derive(Debug, Default)]
pub struct Outcome {
    pub errors: Vec<String>,
    pub elapsed: Duration,
    /// `type="module"` scripts skipped (ES modules are a later phase).
    pub modules_skipped: usize,
    /// A script tripped an engine bug; remaining page JS was abandoned.
    pub panicked: bool,
    /// Page-initiated network requests (fetch/XHR) performed.
    pub fetches: usize,
    /// Captured console.* output, for tests and debugging.
    pub console: Vec<String>,
}

impl Outcome {
    /// The headline failure for `app.notice`, if anything went wrong.
    pub fn notice(&self) -> Option<String> {
        let first = self.errors.first()?;
        match self.errors.len() {
            1 => Some(format!("JS: {first}")),
            n => Some(format!("JS: {first} (+{} more)", n - 1)),
        }
    }
}

/// Boa's job executor for a page. It is the bundled `SimpleJobExecutor`
/// with one decisive change: it **parks** on in-flight async jobs
/// (`group.next().await`) instead of busy-polling them. That is what lets
/// page fetches overlap — they all sit in one `FuturesUnordered`, polled
/// concurrently and driven by the tokio reactor, while the JS thread
/// sleeps until one completes. (The stock executor spins a core the whole
/// time a request is outstanding; we cannot ship that.) Timers are
/// virtual (driven by `__trust.tick`, not Boa's clock), so `TimeoutJob`s
/// never actually arise here, but we run any stray non-promise job rather
/// than drop it.
#[derive(Default)]
struct PageJobExecutor {
    promise_jobs: RefCell<VecDeque<PromiseJob>>,
    async_jobs: RefCell<VecDeque<NativeAsyncJob>>,
    other_jobs: RefCell<VecDeque<Job>>,
}

impl PageJobExecutor {
    fn clear(&self) {
        self.promise_jobs.borrow_mut().clear();
        self.async_jobs.borrow_mut().clear();
        self.other_jobs.borrow_mut().clear();
    }
}

impl JobExecutor for PageJobExecutor {
    fn enqueue_job(self: Rc<Self>, job: Job, _ctx: &mut Context) {
        match job {
            Job::PromiseJob(p) => self.promise_jobs.borrow_mut().push_back(p),
            Job::AsyncJob(a) => self.async_jobs.borrow_mut().push_back(a),
            other => self.other_jobs.borrow_mut().push_back(other),
        }
    }

    fn run_jobs(self: Rc<Self>, ctx: &mut Context) -> JsResult<()> {
        // Reached by no-net pages (and unit tests): no tokio-backed async
        // jobs exist, so `run_jobs_async` never parks on the reactor — it
        // runs the synchronous queues straight through, so any block_on
        // suffices (no tokio runtime needed).
        futures::executor::block_on(self.run_jobs_async(&RefCell::new(ctx)))
    }

    async fn run_jobs_async(self: Rc<Self>, context: &RefCell<&mut Context>) -> JsResult<()>
    where
        Self: Sized,
    {
        use futures::stream::StreamExt as _;
        let mut group = futures::stream::FuturesUnordered::new();
        loop {
            // Newly enqueued fetches join the concurrently-polled set.
            for job in std::mem::take(&mut *self.async_jobs.borrow_mut()) {
                group.push(job.call(context));
            }
            // Drain every synchronous job that's ready now. These run to
            // completion one at a time (spec: one job at a time) and may
            // enqueue more jobs (a settled fetch enqueues its `.then`s).
            let promise = std::mem::take(&mut *self.promise_jobs.borrow_mut());
            let other = std::mem::take(&mut *self.other_jobs.borrow_mut());
            let ran_sync = !promise.is_empty() || !other.is_empty();
            for job in promise {
                if let Err(err) = job.call(&mut context.borrow_mut()) {
                    self.clear();
                    return Err(err);
                }
            }
            for job in other {
                let r = match job {
                    Job::TimeoutJob(t) => t.call(&mut context.borrow_mut()),
                    Job::GenericJob(g) => g.call(&mut context.borrow_mut()),
                    Job::PromiseJob(p) => p.call(&mut context.borrow_mut()),
                    Job::AsyncJob(a) => {
                        group.push(a.call(context));
                        Ok(JsValue::undefined())
                    }
                    _ => Ok(JsValue::undefined()), // Job is #[non_exhaustive]
                };
                if let Err(err) = r {
                    self.clear();
                    return Err(err);
                }
            }
            // Everything drained and nothing in flight: quiescent.
            if self.async_jobs.borrow().is_empty() && !ran_sync && group.is_empty() {
                break;
            }
            // Fresh jobs appeared while we worked: service them before
            // parking, so we never sleep with runnable work pending.
            if ran_sync || !self.async_jobs.borrow().is_empty() {
                continue;
            }
            // Park until the next in-flight fetch resolves. The reactor
            // wakes us — no spin, no idle CPU. Its completion enqueues the
            // promise `.then` jobs we'll run on the next turn.
            if let Some(Err(err)) = group.next().await {
                self.clear();
                return Err(err);
            }
            context.borrow_mut().clear_kept_objects();
        }
        Ok(())
    }
}

/// A fresh, isolated engine for one page, with the hostile-input limits
/// applied. Dropped with the page — nothing survives navigation.
pub fn page_context() -> Context {
    page_context_with(None).0
}

fn page_context_with(loader: Option<Rc<WebModuleLoader>>) -> (Context, Rc<PageHooks>) {
    let hooks = Rc::new(PageHooks {
        rejections: RefCell::new(Vec::new()),
    });
    let executor = Rc::new(PageJobExecutor::default());
    let mut ctx = match loader {
        Some(loader) => Context::builder()
            .module_loader(loader)
            .host_hooks(hooks.clone())
            .job_executor(executor)
            .build()
            .unwrap_or_default(),
        None => Context::builder()
            .host_hooks(hooks.clone())
            .job_executor(executor)
            .build()
            .unwrap_or_default(),
    };
    let limits = ctx.runtime_limits_mut();
    limits.set_loop_iteration_limit(LOOP_LIMIT);
    // Recursion (512) and stack-size defaults are kept: deep enough for
    // real bundles, shallow enough to stop runaway recursion fast.
    (ctx, hooks)
}

/// Run one script, tolerating failure: an exception (including a tripped
/// runtime limit) lands in `outcome.errors` tagged with `name`, and the
/// page lives on. Skipped outright when the budget is spent.
pub fn run_script(
    ctx: &mut Context,
    name: &str,
    source: &[u8],
    budget: &Budget,
    outcome: &mut Outcome,
) {
    if budget.exhausted() || outcome.elapsed >= COMPUTE_BUDGET {
        outcome
            .errors
            .push(format!("{name}: skipped, page JS budget exhausted"));
        return;
    }
    let started = Instant::now();
    // catch_unwind: a Boa VM bug must cost one script, not the page —
    // the DOM mutations made so far stay serializable. (RefCell guards
    // release during unwind, so the arena stays consistent.)
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.eval(Source::from_bytes(source))
    }));
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => outcome.errors.push(format!("{name}: {err}")),
        Err(_) => {
            outcome
                .errors
                .push(format!("{name}: engine panic (Boa bug) — page JS halted"));
            outcome.panicked = true;
        }
    }
    outcome.elapsed += started.elapsed();
}

// ---- The DOM syscall boundary ----------------------------------------
//
// The arena lives in Rust; JS sees nodes as bare integer ids. A thin set
// of `__dom_*` globals is the entire Rust↔JS surface — flat, typed, no
// GC entanglement — and the DOM API shape (prototypes, getters, events,
// timers) is built *in JavaScript* by PRELUDE on top of them. Swapping
// engines means re-gluing these syscalls; the prelude ports verbatim.

/// The page's arena, shared with syscalls through the context's
/// host-defined storage.
#[derive(boa_engine::JsData)]
struct PageDom(Rc<RefCell<Dom>>);

impl boa_engine::gc::Finalize for PageDom {}
// SAFETY: holds no GC-managed objects.
unsafe impl boa_engine::gc::Trace for PageDom {
    boa_engine::gc::empty_trace!();
}

/// Session-lifetime, RAM-only web storage: origin-bucketed key/value
/// maps shared across pages, dead with the process — never disk.
pub type WebStorage = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, String>>>,
>;

/// Network access for page JS: requests run on the app's tokio runtime
/// from the JS thread, metered and capped. No CORS theater — there are
/// no cookies or ambient credentials to protect; the real boundary is
/// `http::subresource_allowed` (no private-address pivots) plus caps.
#[derive(boa_engine::JsData)]
struct PageNet {
    handle: tokio::runtime::Handle,
    page: url::Url,
    budget: Rc<Budget>,
    fetched: std::cell::Cell<usize>,
}

impl boa_engine::gc::Finalize for PageNet {}
// SAFETY: holds no GC-managed objects.
unsafe impl boa_engine::gc::Trace for PageNet {
    boa_engine::gc::empty_trace!();
}

/// The storage syscalls' view: the shared map plus this page's origin.
#[derive(boa_engine::JsData)]
struct PageStore {
    map: WebStorage,
    origin: String,
}

impl boa_engine::gc::Finalize for PageStore {}
// SAFETY: holds no GC-managed objects.
unsafe impl boa_engine::gc::Trace for PageStore {
    boa_engine::gc::empty_trace!();
}

/// Most page-initiated requests (fetch/XHR/module loads) per page.
/// Was 24 (Phase 2a); archive.org's home alone needs ~32 module loads
/// before any data — at 24 the app's chunks were cut off mid-graph.
/// 96 covers its full boot (62 observed) with headroom; still a hard
/// envelope against runaway pages. PROVISIONAL — her call to keep.
const MAX_PAGE_FETCHES: usize = 96;

fn page_dom(ctx: &mut Context) -> Rc<RefCell<Dom>> {
    ctx.realm()
        .host_defined()
        .get::<PageDom>()
        .expect("PageDom installed before any syscall")
        .0
        .clone()
}

/// Node id from a JS argument; None for null/undefined/out-of-range.
fn arg_node(dom: &Dom, args: &[JsValue], i: usize) -> Option<usize> {
    let n = args.get(i)?.as_number()?;
    let id = n as usize;
    (n >= 0.0 && dom.is_valid(id)).then_some(id)
}

fn arg_str(args: &[JsValue], i: usize, ctx: &mut Context) -> String {
    args.get(i)
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_lossy())
        .unwrap_or_default()
}

fn id_value(id: Option<usize>) -> JsValue {
    match id {
        Some(id) => JsValue::from(id as f64),
        None => JsValue::null(),
    }
}

fn str_value(s: &str) -> JsValue {
    JsValue::from(JsString::from(s))
}

fn ids_array(ids: Vec<usize>, ctx: &mut Context) -> JsValue {
    JsArray::from_iter(ids.into_iter().map(|i| JsValue::from(i as f64)), ctx).into()
}

/// Register every `__dom_*` / `__url_parse` syscall. Each is a plain fn
/// pointer fetching the arena from host-defined state.
fn register_syscalls(ctx: &mut Context) -> JsResult<()> {
    type Sys = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;
    let table: &[(&str, usize, Sys)] = &[
        ("__dom_create_element", 1, sys_create_element),
        ("__dom_create_text", 1, sys_create_text),
        ("__dom_create_fragment", 0, sys_create_fragment),
        ("__dom_create_comment", 0, sys_create_comment),
        ("__dom_append", 2, sys_append),
        ("__dom_insert_before", 3, sys_insert_before),
        ("__dom_detach", 1, sys_detach),
        ("__dom_parent", 1, sys_parent),
        ("__dom_children", 1, sys_children),
        ("__dom_next", 1, sys_next),
        ("__dom_prev", 1, sys_prev),
        ("__dom_node_type", 1, sys_node_type),
        ("__dom_tag", 1, sys_tag),
        ("__dom_get_attr", 2, sys_get_attr),
        ("__dom_set_attr", 3, sys_set_attr),
        ("__dom_remove_attr", 2, sys_remove_attr),
        ("__dom_attr_names", 1, sys_attr_names),
        ("__dom_text", 1, sys_text),
        ("__dom_set_text", 2, sys_set_text),
        ("__dom_inner_html", 1, sys_inner_html),
        ("__dom_set_inner_html", 2, sys_set_inner_html),
        ("__dom_outer_html", 1, sys_outer_html),
        ("__dom_insert_adjacent", 3, sys_insert_adjacent),
        ("__dom_query", 3, sys_query),
        ("__dom_matches", 2, sys_matches),
        ("__dom_get_by_id", 1, sys_get_by_id),
        ("__dom_clone", 2, sys_clone),
        ("__dom_doc_element", 0, sys_doc_element),
        ("__url_parse", 2, sys_url_parse),
        ("__dom_attach_shadow", 1, sys_attach_shadow),
        ("__dom_shadow_root", 1, sys_shadow_root),
        ("__dom_adopt_styles", 2, sys_adopt_styles),
        ("__dom_template_content", 1, sys_template_content),
        ("__http_fetch", 4, sys_http_fetch),
        ("__http_fetch_async", 4, sys_http_fetch_async),
        ("__cookie_get", 0, sys_cookie_get),
        ("__cookie_set", 1, sys_cookie_set),
        ("__storage_get", 2, sys_storage_get),
        ("__storage_set", 3, sys_storage_set),
        ("__storage_remove", 2, sys_storage_remove),
        ("__storage_clear", 1, sys_storage_clear),
        ("__storage_key", 2, sys_storage_key),
        ("__storage_len", 1, sys_storage_len),
    ];
    for (name, len, f) in table {
        ctx.register_global_callable(JsString::from(*name), *len, NativeFunction::from_fn_ptr(*f))?;
    }
    Ok(())
}

fn sys_create_element(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let tag = arg_str(args, 0, ctx);
    let dom = page_dom(ctx);
    let id = dom.borrow_mut().create_element(&tag);
    Ok(id_value(Some(id)))
}

fn sys_create_text(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = arg_str(args, 0, ctx);
    let dom = page_dom(ctx);
    let id = dom.borrow_mut().create_text(&text);
    Ok(id_value(Some(id)))
}

fn sys_create_fragment(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let id = dom.borrow_mut().create_fragment();
    Ok(id_value(Some(id)))
}

fn sys_create_comment(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = arg_str(args, 0, ctx);
    let dom = page_dom(ctx);
    let id = dom.borrow_mut().create_comment(&text);
    Ok(id_value(Some(id)))
}

fn sys_append(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    if let (Some(p), Some(c)) = (arg_node(&d, args, 0), arg_node(&d, args, 1)) {
        d.append(p, c);
    }
    Ok(JsValue::undefined())
}

fn sys_insert_before(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    if let (Some(p), Some(c)) = (arg_node(&d, args, 0), arg_node(&d, args, 1)) {
        let r = arg_node(&d, args, 2);
        d.insert_before(p, c, r);
    }
    Ok(JsValue::undefined())
}

fn sys_detach(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    if let Some(id) = arg_node(&d, args, 0) {
        d.detach(id);
    }
    Ok(JsValue::undefined())
}

fn sys_parent(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(id_value(
        arg_node(&d, args, 0).and_then(|id| d.node(id).parent),
    ))
}

fn sys_children(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let ids = {
        let d = dom.borrow();
        arg_node(&d, args, 0)
            .map(|id| d.children(id))
            .unwrap_or_default()
    };
    Ok(ids_array(ids, ctx))
}

fn sys_next(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(id_value(
        arg_node(&d, args, 0).and_then(|id| d.node(id).next_sibling),
    ))
}

fn sys_prev(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(id_value(
        arg_node(&d, args, 0).and_then(|id| d.node(id).prev_sibling),
    ))
}

fn sys_node_type(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use crate::dom::NodeData;
    let dom = page_dom(ctx);
    let d = dom.borrow();
    let t = match arg_node(&d, args, 0).map(|id| &d.node(id).data) {
        Some(NodeData::Element { .. }) => 1,
        Some(NodeData::Text(_)) => 3,
        Some(NodeData::Comment(_)) => 8,
        Some(NodeData::Document) => 9,
        Some(NodeData::Doctype) => 10,
        Some(NodeData::Fragment) => 11,
        None => 0,
    };
    Ok(JsValue::from(t))
}

fn sys_tag(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(match arg_node(&d, args, 0).and_then(|id| d.tag_name(id)) {
        Some(t) => str_value(t),
        None => JsValue::null(),
    })
}

fn sys_get_attr(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_str(args, 1, ctx);
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(
        match arg_node(&d, args, 0).and_then(|id| d.attr(id, &name)) {
            Some(v) => str_value(v),
            None => JsValue::null(),
        },
    )
}

fn sys_set_attr(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_str(args, 1, ctx);
    let value = arg_str(args, 2, ctx);
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    if let Some(id) = arg_node(&d, args, 0) {
        d.set_attr(id, &name, &value);
    }
    Ok(JsValue::undefined())
}

fn sys_remove_attr(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_str(args, 1, ctx);
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    if let Some(id) = arg_node(&d, args, 0) {
        d.remove_attr(id, &name);
    }
    Ok(JsValue::undefined())
}

fn sys_attr_names(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let names = {
        let d = dom.borrow();
        arg_node(&d, args, 0)
            .map(|id| d.attr_names(id))
            .unwrap_or_default()
    };
    let vals: Vec<JsValue> = names.iter().map(|n| str_value(n)).collect();
    Ok(JsArray::from_iter(vals, ctx).into())
}

fn sys_text(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    let t = arg_node(&d, args, 0)
        .map(|id| match d.comment_text(id) {
            Some(c) => c.to_string(),
            None => d.text_content(id),
        })
        .unwrap_or_default();
    Ok(str_value(&t))
}

fn sys_set_text(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = arg_str(args, 1, ctx);
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    if let Some(id) = arg_node(&d, args, 0) {
        if d.comment_text(id).is_some() {
            d.set_comment_text(id, &text);
        } else {
            d.set_text(id, &text);
        }
    }
    Ok(JsValue::undefined())
}

fn sys_inner_html(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    let h = arg_node(&d, args, 0)
        .map(|id| d.inner_html(id))
        .unwrap_or_default();
    Ok(str_value(&h))
}

fn sys_set_inner_html(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let html = arg_str(args, 1, ctx);
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    if let Some(id) = arg_node(&d, args, 0) {
        let context_tag = d.tag_name(id).unwrap_or("div").to_string();
        // A template's markup lands in its content fragment, where
        // `template.content` (and Lit) expect it.
        let target = d.content_target(id);
        for c in d.children(target) {
            d.detach(c);
        }
        for n in d.parse_fragment_into(&context_tag, &html) {
            d.append(target, n);
        }
    }
    Ok(JsValue::undefined())
}

fn sys_outer_html(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    let h = arg_node(&d, args, 0)
        .map(|id| d.serialize(id))
        .unwrap_or_default();
    Ok(str_value(&h))
}

fn sys_insert_adjacent(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let position = arg_str(args, 1, ctx);
    let html = arg_str(args, 2, ctx);
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    let Some(id) = arg_node(&d, args, 0) else {
        return Ok(JsValue::undefined());
    };
    let context_tag = match position.as_str() {
        "beforebegin" | "afterend" => d
            .node(id)
            .parent
            .and_then(|p| d.tag_name(p))
            .unwrap_or("div")
            .to_string(),
        _ => d.tag_name(id).unwrap_or("div").to_string(),
    };
    let nodes = d.parse_fragment_into(&context_tag, &html);
    match position.as_str() {
        "afterbegin" => {
            let first = d.node(id).first_child;
            for n in nodes {
                d.insert_before(id, n, first);
            }
        }
        "beforebegin" => {
            if let Some(p) = d.node(id).parent {
                for n in nodes {
                    d.insert_before(p, n, Some(id));
                }
            }
        }
        "afterend" => {
            if let Some(p) = d.node(id).parent {
                let after = d.node(id).next_sibling;
                for n in nodes {
                    d.insert_before(p, n, after);
                }
            }
        }
        // "beforeend" and anything unrecognized: append.
        _ => {
            for n in nodes {
                d.append(id, n);
            }
        }
    }
    Ok(JsValue::undefined())
}

fn sys_query(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let selector = arg_str(args, 1, ctx);
    let first_only = args.get(2).is_some_and(JsValue::to_boolean);
    let dom = page_dom(ctx);
    let ids = {
        let d = dom.borrow();
        match (arg_node(&d, args, 0), SelectorList::parse(&selector)) {
            (Some(root), Some(sel)) => d.query(root, &sel, first_only),
            _ => Vec::new(),
        }
    };
    Ok(ids_array(ids, ctx))
}

fn sys_matches(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let selector = arg_str(args, 1, ctx);
    let dom = page_dom(ctx);
    let d = dom.borrow();
    let hit = match (arg_node(&d, args, 0), SelectorList::parse(&selector)) {
        (Some(id), Some(sel)) => d.matches(id, &sel),
        _ => false,
    };
    Ok(JsValue::from(hit))
}

fn sys_get_by_id(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let target = arg_str(args, 0, ctx);
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(id_value(d.get_by_id(&target)))
}

fn sys_clone(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    let deep = args.get(1).is_some_and(JsValue::to_boolean);
    Ok(id_value(
        arg_node(&d, args, 0).map(|id| d.clone_subtree(id, deep)),
    ))
}

fn sys_attach_shadow(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    Ok(id_value(
        arg_node(&d, args, 0).map(|id| d.attach_shadow(id)),
    ))
}

fn sys_shadow_root(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(id_value(
        arg_node(&d, args, 0).and_then(|id| d.shadow_root(id)),
    ))
}

/// `__dom_adopt_styles(scope, cssText)`: the prelude pushes a scope's
/// joined adoptedStyleSheets text into the visibility cascade.
fn sys_adopt_styles(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let css = arg_str(args, 1, ctx);
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    if let Some(scope) = arg_node(&d, args, 0) {
        d.set_adopted_styles(scope, &css);
    }
    Ok(JsValue::undefined())
}

fn sys_template_content(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(id_value(
        arg_node(&d, args, 0).map(|id| d.content_target(id)),
    ))
}

fn sys_doc_element(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(id_value(
        d.children(DOCUMENT)
            .into_iter()
            .find(|&c| d.tag_name(c) == Some("html")),
    ))
}

/// `__url_parse(href, base|null)` → array of
/// [href, protocol, host, hostname, port, pathname, search, hash, origin]
/// or null — the workhorse behind both the URL class and absolutized
/// href/src getters.
fn sys_url_parse(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let href = arg_str(args, 0, ctx);
    let base = args
        .get(1)
        .filter(|v| !v.is_null_or_undefined())
        .map(|_| arg_str(args, 1, ctx));
    let parsed = match base {
        Some(b) => url::Url::parse(&b).and_then(|b| b.join(&href)),
        None => url::Url::parse(&href),
    };
    let Ok(u) = parsed else {
        return Ok(JsValue::null());
    };
    let host = match (u.host_str(), u.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    };
    let parts = [
        u.as_str().to_string(),
        format!("{}:", u.scheme()),
        host,
        u.host_str().unwrap_or("").to_string(),
        u.port().map(|p| p.to_string()).unwrap_or_default(),
        u.path().to_string(),
        u.query().map(|q| format!("?{q}")).unwrap_or_default(),
        u.fragment().map(|f| format!("#{f}")).unwrap_or_default(),
        u.origin().ascii_serialization(),
    ];
    let vals: Vec<JsValue> = parts.iter().map(|p| str_value(p)).collect();
    Ok(JsArray::from_iter(vals, ctx).into())
}

/// Run the synchronous half of a page fetch: resolve against the page
/// URL, enforce the scheme/private-space checks, the per-page cap, and
/// the budget, count it, and hand back what's needed to do the request.
/// None = blocked, capped, past budget, or no net grant at all. Touches
/// the network not at all — the actual request is awaited by the caller.
fn page_net_prepare(
    ctx: &mut Context,
    target: &str,
    method: String,
    body: Option<(String, Vec<u8>)>,
) -> Option<(tokio::runtime::Handle, crate::http::Request)> {
    let host = ctx.realm().host_defined();
    let net = host.get::<PageNet>()?;
    let resolved = net.page.join(target).ok()?;
    if !matches!(resolved.scheme(), "http" | "https")
        || !crate::http::subresource_allowed(&net.page, &resolved)
        || net.fetched.get() >= MAX_PAGE_FETCHES
        || net.budget.exhausted()
    {
        return None;
    }
    net.fetched.set(net.fetched.get() + 1);
    let request = crate::http::Request {
        method,
        url: resolved,
        body,
    };
    Some((net.handle.clone(), request))
}

/// Synchronous fetch — for the one caller that genuinely must block:
/// legacy synchronous XHR (`open(..., false)`). Blocks the page thread
/// while the app runtime does the request. Everything else goes through
/// the async path so requests overlap.
fn page_net_fetch(
    ctx: &mut Context,
    target: &str,
    method: String,
    body: Option<(String, Vec<u8>)>,
) -> Option<crate::http::Response> {
    let (handle, request) = page_net_prepare(ctx, target, method, body)?;
    handle.block_on(crate::http::fetch(&request)).ok()
}

/// `__http_fetch(url, method, body|null, content_type|null)` →
/// `[status, content_type, body_text]` or null (blocked/failed/no net).
/// Synchronous from the page's view; the request runs on the tokio
/// runtime while the JS thread blocks. The one blocking caller is legacy
/// synchronous XHR — async fetch/XHR go through `sys_http_fetch_async`.
fn sys_http_fetch(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (url_arg, method, body) = fetch_args(args, ctx);
    Ok(match page_net_fetch(ctx, &url_arg, method, body) {
        Some(resp) => response_to_array(&resp, ctx),
        None => JsValue::null(),
    })
}

/// `__http_fetch_async(url, method, body|null, content_type|null)` →
/// a Promise resolving to `[status, content_type, body_text]`, or null
/// (blocked/capped/failed/no net). The request is dispatched as a Boa
/// async job: it fires immediately and is awaited on the tokio runtime
/// WITHOUT blocking the JS thread, so many in-flight requests overlap
/// (Boa's job executor polls them concurrently). This is the whole
/// reason `Promise.all([fetch(a), fetch(b)])` now runs in parallel.
fn sys_http_fetch_async(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (url_arg, method, body) = fetch_args(args, ctx);
    let (promise, resolvers) = JsPromise::new_pending(ctx);
    match page_net_prepare(ctx, &url_arg, method, body) {
        None => {
            // Blocked/capped/no net: settle to null now, matching the
            // sync syscall's contract (the prelude turns null into a
            // rejected fetch / failed XHR).
            let _ = resolvers
                .resolve
                .call(&JsValue::undefined(), &[JsValue::null()], ctx);
        }
        Some((_handle, request)) => {
            let realm = ctx.realm().clone();
            let job = NativeAsyncJob::with_realm(
                async move |ctx_cell: &RefCell<&mut Context>| {
                    // Await the request WITHOUT borrowing the context —
                    // holding the borrow across the await would serialize
                    // every fetch again. The executor polls all of these
                    // concurrently; we touch the context only to settle
                    // the promise once the bytes are in.
                    let result = crate::http::fetch(&request).await;
                    let mut guard = ctx_cell.borrow_mut();
                    let value = match result {
                        Ok(resp) => response_to_array(&resp, &mut guard),
                        Err(_) => JsValue::null(),
                    };
                    let _ = resolvers
                        .resolve
                        .call(&JsValue::undefined(), &[value], &mut guard);
                    Ok(JsValue::undefined())
                },
                realm,
            );
            ctx.enqueue_job(job.into());
        }
    }
    Ok(promise.into())
}

/// `document.cookie` getter: the jar's name=value pairs for this page
/// (read-only RAM jar; see http.rs). Empty without a net grant.
fn sys_cookie_get(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let cookies = ctx
        .realm()
        .host_defined()
        .get::<PageNet>()
        .map(|n| crate::http::cookies_for_js(&n.page))
        .unwrap_or_default();
    Ok(str_value(&cookies))
}

/// `document.cookie = "..."`: store in the RAM jar so later reads see it.
/// Never sent to a server (read-only jar).
fn sys_cookie_set(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let line = arg_str(args, 0, ctx);
    if let Some(page) = ctx
        .realm()
        .host_defined()
        .get::<PageNet>()
        .map(|n| n.page.clone())
    {
        crate::http::set_cookie_from_js(&page, &line);
    }
    Ok(JsValue::undefined())
}

/// Parse the shared `(url, method, body)` arguments of the fetch
/// syscalls: method normalized, body paired with its content type.
fn fetch_args(args: &[JsValue], ctx: &mut Context) -> (String, String, Option<(String, Vec<u8>)>) {
    let url_arg = arg_str(args, 0, ctx);
    let mut method: String = arg_str(args, 1, ctx)
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect();
    method.make_ascii_uppercase();
    if method.is_empty() {
        method = String::from("GET");
    }
    let body = args
        .get(2)
        .filter(|v| !v.is_null_or_undefined())
        .map(|_| arg_str(args, 2, ctx));
    let content_type = args
        .get(3)
        .filter(|v| !v.is_null_or_undefined())
        .map(|_| arg_str(args, 3, ctx));
    let body = body.map(|b| {
        (
            content_type.unwrap_or_else(|| String::from("text/plain;charset=UTF-8")),
            b.into_bytes(),
        )
    });
    (url_arg, method, body)
}

/// A fetched response as the `[status, content_type, body_text]` array
/// the prelude expects.
fn response_to_array(resp: &crate::http::Response, ctx: &mut Context) -> JsValue {
    let vals = vec![
        JsValue::from(f64::from(resp.status)),
        str_value(&resp.content_type),
        str_value(&String::from_utf8_lossy(&resp.body)),
    ];
    JsArray::from_iter(vals, ctx).into()
}

fn store_bucket(ctx: &mut Context, args: &[JsValue]) -> Option<(WebStorage, String)> {
    let kind = arg_str(args, 0, ctx);
    let host = ctx.realm().host_defined();
    let store = host.get::<PageStore>()?;
    Some((store.map.clone(), format!("{kind}:{}", store.origin)))
}

fn sys_storage_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = arg_str(args, 1, ctx);
    let Some((map, bucket)) = store_bucket(ctx, args) else {
        return Ok(JsValue::null());
    };
    let map = map.lock().unwrap();
    Ok(match map.get(&bucket).and_then(|b| b.get(&key)) {
        Some(v) => str_value(v),
        None => JsValue::null(),
    })
}

fn sys_storage_set(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = arg_str(args, 1, ctx);
    let value = arg_str(args, 2, ctx);
    if let Some((map, bucket)) = store_bucket(ctx, args) {
        map.lock()
            .unwrap()
            .entry(bucket)
            .or_default()
            .insert(key, value);
    }
    Ok(JsValue::undefined())
}

fn sys_storage_remove(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = arg_str(args, 1, ctx);
    if let Some((map, bucket)) = store_bucket(ctx, args)
        && let Some(b) = map.lock().unwrap().get_mut(&bucket)
    {
        b.remove(&key);
    }
    Ok(JsValue::undefined())
}

fn sys_storage_clear(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some((map, bucket)) = store_bucket(ctx, args) {
        map.lock().unwrap().remove(&bucket);
    }
    Ok(JsValue::undefined())
}

fn sys_storage_key(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let index = args.get(1).and_then(JsValue::as_number).unwrap_or(-1.0);
    let Some((map, bucket)) = store_bucket(ctx, args) else {
        return Ok(JsValue::null());
    };
    let map = map.lock().unwrap();
    let key = (index >= 0.0)
        .then(|| {
            map.get(&bucket)
                .and_then(|b| b.keys().nth(index as usize).cloned())
        })
        .flatten();
    Ok(match key {
        Some(k) => str_value(&k),
        None => JsValue::null(),
    })
}

fn sys_storage_len(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some((map, bucket)) = store_bucket(ctx, args) else {
        return Ok(JsValue::from(0));
    };
    let n = map.lock().unwrap().get(&bucket).map_or(0, |b| b.len());
    Ok(JsValue::from(n as f64))
}

// ---- Page transformation ---------------------------------------------

/// Scripts the page may execute as classic scripts.
fn is_classic(type_attr: &Option<String>) -> bool {
    match type_attr {
        None => true,
        Some(t) => matches!(
            t.trim().to_ascii_lowercase().as_str(),
            "" | "text/javascript" | "application/javascript" | "text/ecmascript"
        ),
    }
}

/// Most settle-loop timer callbacks per page, beyond which we declare
/// the page non-quiescent and render what exists.
const MAX_TICKS: usize = 200;

/// Everything a page transformation needs from the outside world.
pub struct PageEnv {
    pub url: String,
    pub viewport: (u16, u16),
    /// Pre-fetched external scripts keyed by raw `src` attribute
    /// (None = the fetch failed).
    pub externals: Vec<(String, Option<Vec<u8>>)>,
    /// Pre-fetched `<link rel=stylesheet>` bodies keyed by raw `href`,
    /// for the display/visibility cascade (failed fetches are absent —
    /// fail-open, the page just renders un-hidden).
    pub sheets: Vec<(String, String)>,
    /// Pre-fetched module bodies keyed by RESOLVED absolute URL
    /// (modulepreload hints + module entry srcs), consumed by the
    /// module loader before it ever touches the network.
    pub preloaded: Vec<(String, Vec<u8>)>,
    /// Runtime handle for page-initiated fetch/XHR; None = pages get no
    /// network at all (every request resolves to failure).
    pub net: Option<tokio::runtime::Handle>,
    /// Shared session storage; None gets a fresh page-lifetime map.
    pub storage: Option<WebStorage>,
}

impl PageEnv {
    /// No network, no shared storage: the test/diagnostic default.
    pub fn bare(url: &str) -> Self {
        Self {
            url: url.to_string(),
            viewport: (80, 24),
            externals: Vec::new(),
            sheets: Vec::new(),
            preloaded: Vec::new(),
            net: None,
            storage: None,
        }
    }
}

/// Run a page's scripts against a real DOM and return the post-JS HTML.
/// Never fails: any error lands in the Outcome and the best available
/// document is returned.
/// Surfaces unhandled promise rejections — async component updates
/// (Lit's whole update path) fail THROUGH these, invisibly otherwise.
/// Reject records the reason; a later Handle retracts it; whatever
/// remains at drain time was genuinely unhandled.
struct PageHooks {
    rejections: RefCell<Vec<(boa_engine::JsObject, String)>>,
}

impl boa_engine::context::HostHooks for PageHooks {
    fn promise_rejection_tracker(
        &self,
        promise: &boa_engine::JsObject,
        operation: boa_engine::builtins::promise::OperationType,
        context: &mut Context,
    ) {
        use boa_engine::builtins::promise::{OperationType, PromiseState};
        match operation {
            OperationType::Reject => {
                let reason = boa_engine::object::builtins::JsPromise::from_object(promise.clone())
                    .ok()
                    .map(|p| match p.state() {
                        PromiseState::Rejected(v) => {
                            let mut s = format!("{}", v.display());
                            // Error objects carry a stack — diagnostics
                            // are worthless without it.
                            if let Some(o) = v.as_object()
                                && let Ok(st) = o.get(boa_engine::js_string!("stack"), context)
                                && !st.is_undefined()
                                && !st.is_null()
                                && let Ok(st) = st.to_string(context)
                                && !st.is_empty()
                            {
                                s.push_str(&format!("\n{}", st.to_std_string_lossy()));
                            }
                            s
                        }
                        _ => String::from("(rejection pending)"),
                    })
                    .unwrap_or_else(|| String::from("(unknown rejection)"));
                let mut pending = self.rejections.borrow_mut();
                if pending.len() < 32 {
                    pending.push((promise.clone(), reason));
                }
            }
            OperationType::Handle => {
                self.rejections
                    .borrow_mut()
                    .retain(|(p, _)| !boa_engine::JsObject::equals(p, promise));
            }
        }
    }
}

/// ES modules over the web: imports resolve against the importing
/// module's URL (carried as the Source path), fetch through the page's
/// net grant with the same caps and guards as everything else, and
/// cache per page. Bare specifiers ("lit") have no resolution here and
/// reject — honestly.
struct WebModuleLoader {
    page: Option<url::Url>,
    cache: RefCell<std::collections::HashMap<String, boa_engine::Module>>,
    /// Bodies fetched in parallel before the engine started (taken on
    /// first use; a miss falls back to the network).
    preloaded: Rc<RefCell<std::collections::HashMap<String, Vec<u8>>>>,
}

impl boa_engine::module::ModuleLoader for WebModuleLoader {
    async fn load_imported_module(
        self: Rc<Self>,
        referrer: boa_engine::module::Referrer,
        specifier: JsString,
        context: &RefCell<&mut Context>,
    ) -> JsResult<boa_engine::Module> {
        let spec = specifier.to_std_string_lossy();
        // The HTML spec's rule: only absolute URLs and /-, ./-, ../-
        // prefixed paths resolve. Bare specifiers ("lit") need an
        // import map we don't have — reject, don't guess.
        if !(spec.starts_with('/')
            || spec.starts_with("./")
            || spec.starts_with("../")
            || url::Url::parse(&spec).is_ok())
        {
            return Err(boa_engine::JsNativeError::typ()
                .with_message(format!("cannot resolve module specifier '{spec}'"))
                .into());
        }
        let base = referrer
            .path()
            .and_then(|p| p.to_str())
            .and_then(|s| url::Url::parse(s).ok())
            .or_else(|| self.page.clone());
        let resolved = base
            .as_ref()
            .and_then(|b| b.join(&spec).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| {
                boa_engine::JsNativeError::typ()
                    .with_message(format!("cannot resolve module specifier '{spec}'"))
            })?;
        let key = resolved.to_string();
        if let Some(cached) = self.cache.borrow().get(&key) {
            return Ok(cached.clone());
        }
        let preloaded = self.preloaded.borrow_mut().remove(&key);
        let body = match preloaded {
            Some(body) => body,
            None => {
                // Module loads are atomic from Boa's view: this future must
                // not yield to another module load while a fetch is in
                // flight, or interleaved loads cross up Boa's per-referrer
                // loaded_modules. We can't `block_on` here (we're inside the
                // job loop's `block_on`; nesting tokio runtimes panics), so
                // spawn the request onto the runtime and block the page
                // thread on a plain channel. Page fetch/XHR keep their own
                // async path and still overlap.
                let fail = || {
                    boa_engine::JsNativeError::typ()
                        .with_message(format!("module fetch failed or blocked: {key}"))
                };
                let (handle, request) = {
                    let mut ctx = context.borrow_mut();
                    page_net_prepare(&mut ctx, &key, String::from("GET"), None)
                }
                .ok_or_else(fail)?;
                let (tx, rx) = std::sync::mpsc::channel();
                handle.spawn(async move {
                    let _ = tx.send(crate::http::fetch(&request).await);
                });
                rx.recv().map_err(|_| fail())?.map_err(|_| fail())?.body
            }
        };
        let module = {
            let mut ctx = context.borrow_mut();
            boa_engine::Module::parse(
                Source::from_bytes(&body).with_path(std::path::Path::new(&key)),
                None,
                &mut ctx,
            )?
        };
        self.cache.borrow_mut().insert(key, module.clone());
        Ok(module)
    }
}

/// Run one parsed module to completion: load (imports fetch through the
/// loader), link, evaluate, driving the job queue until the promise
/// settles or the budget calls time.
fn run_module(
    ctx: &mut Context,
    name: &str,
    source: &[u8],
    path: &str,
    budget: &Budget,
    outcome: &mut Outcome,
) {
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        boa_engine::Module::parse(
            Source::from_bytes(source).with_path(std::path::Path::new(path)),
            None,
            ctx,
        )
    }));
    let module = match parsed {
        Ok(Ok(m)) => m,
        Ok(Err(err)) => {
            outcome.errors.push(format!("{name}: {err}"));
            return;
        }
        Err(_) => {
            outcome
                .errors
                .push(format!("{name}: engine panic (Boa bug) — page JS halted"));
            outcome.panicked = true;
            return;
        }
    };
    let evaluated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let promise = module.load_link_evaluate(ctx);
        // Drive load/link/evaluate — and any fetches the module's
        // top-level code kicks off — to quiescence under the runtime
        // (concurrent, budget-bounded), then read the settled state.
        run_jobs_into(ctx, budget, outcome);
        match promise.state() {
            boa_engine::builtins::promise::PromiseState::Pending => {
                Some(String::from("module still pending at budget end"))
            }
            boa_engine::builtins::promise::PromiseState::Fulfilled(_) => None,
            boa_engine::builtins::promise::PromiseState::Rejected(err) => {
                Some(format!("{}", err.display()))
            }
        }
    }));
    match evaluated {
        Ok(None) => {}
        Ok(Some(err)) => outcome.errors.push(format!("{name}: {err}")),
        Err(_) => {
            outcome
                .errors
                .push(format!("{name}: engine panic (Boa bug) — page JS halted"));
            outcome.panicked = true;
        }
    }
}

/// A page loaded and settled: the engine + arena, alive. The one-shot
/// `transform` serializes and drops it; the page actor keeps it and
/// dispatches events into it.
struct LoadedPage {
    ctx: Context,
    dom: Rc<RefCell<Dom>>,
    budget: Rc<Budget>,
    outcome: Outcome,
    started: Instant,
    page_url: Option<url::Url>,
    hooks: Rc<PageHooks>,
}

/// Parse, run scripts, fire lifecycle, settle. Err(outcome) means
/// "render the original HTML with this outcome" (no scripts, or our
/// own plumbing failed).
fn load_page(html: &str, env: &PageEnv) -> Result<LoadedPage, Outcome> {
    let page_url = env.url.as_str();
    let viewport = env.viewport;
    let externals = &env.externals;
    let mut outcome = Outcome::default();
    let dom = Rc::new(RefCell::new(Dom::parse_document(html)));
    if !env.sheets.is_empty() {
        dom.borrow_mut().attach_external_sheets(&env.sheets);
    }
    let scripts = dom.borrow().scripts();
    if scripts.is_empty() {
        // No JS ran: the original bytes stand (so <noscript> content
        // still renders, as it should).
        return Err(outcome);
    }

    let started = Instant::now();
    let parsed_url = url::Url::parse(page_url).ok();
    let preloaded: Rc<RefCell<std::collections::HashMap<String, Vec<u8>>>> =
        Rc::new(RefCell::new(env.preloaded.iter().cloned().collect()));
    let loader = Rc::new(WebModuleLoader {
        page: parsed_url.clone(),
        cache: RefCell::new(std::collections::HashMap::new()),
        preloaded: preloaded.clone(),
    });
    let (mut ctx, hooks) = page_context_with(Some(loader));
    let budget = Rc::new(Budget::new(WALL_BUDGET));
    {
        let mut host = ctx.realm().host_defined_mut();
        host.insert(PageDom(dom.clone()));
        host.insert(PageStore {
            map: env.storage.clone().unwrap_or_default(),
            origin: parsed_url
                .as_ref()
                .map(|u| u.origin().ascii_serialization())
                .unwrap_or_else(|| String::from("null")),
        });
        if let (Some(handle), Some(page)) = (env.net.clone(), parsed_url.clone()) {
            host.insert(PageNet {
                handle,
                page,
                budget: budget.clone(),
                fetched: std::cell::Cell::new(0),
            });
        }
    }
    if let Err(err) = register_syscalls(&mut ctx) {
        outcome.errors.push(format!("syscalls: {err}"));
        return Err(outcome);
    }
    // Nominal pixel viewport for scripts that measure: 8x16 cells.
    let cfg = format!(
        "globalThis.__trust_cfg = {{ url: \"{}\", ua: \"TRust/0.1\", width: {}, height: {} }};",
        esc_js(page_url),
        u32::from(viewport.0) * 8,
        u32::from(viewport.1) * 16,
    );
    run_script(&mut ctx, "config", cfg.as_bytes(), &budget, &mut outcome);
    run_script(
        &mut ctx,
        "prelude",
        PRELUDE.as_bytes(),
        &budget,
        &mut outcome,
    );
    if !outcome.errors.is_empty() {
        // The prelude is ours: if it broke, render without JS and say so.
        return Err(outcome);
    }

    for (i, (src, inline, type_attr)) in scripts.iter().enumerate() {
        if !is_classic(type_attr) {
            // ES modules execute for real now; non-module foreign types
            // (importmap, json, ...) still skip.
            if type_attr.as_deref().is_some_and(|t| t.trim() == "module") {
                match src {
                    Some(src) => {
                        // Load the entry module THROUGH the loader via a
                        // synthetic importer — do NOT parse + evaluate it
                        // directly. A module run as the "entry" (its own
                        // `load_link_evaluate`) is tracked differently by
                        // Boa than a loader-provided one. archive.org
                        // dynamically imports its OWN entry URL, and the
                        // mismatch leaves Boa with two Module records for
                        // one specifier — tripping an internal identity
                        // assert whose panic then stack-overflows
                        // formatting the cyclic module graph (= abort).
                        // Routing the entry through the loader keeps one
                        // tracked Module. The loader pulls the body from
                        // the preload set (left in place for it) or the net.
                        let resolved = parsed_url
                            .as_ref()
                            .and_then(|b| b.join(src).ok())
                            .map(|u| u.to_string());
                        let pre = resolved
                            .as_ref()
                            .and_then(|k| preloaded.borrow_mut().remove(k));
                        match pre {
                            Some(body) => {
                                let path = resolved.as_deref().unwrap_or(src.as_str());
                                run_module(&mut ctx, src, &body, path, &budget, &mut outcome);
                            }
                            None => {
                                match page_net_fetch(&mut ctx, src, String::from("GET"), None) {
                                    Some(resp) => {
                                        let path = resp.url.to_string();
                                        run_module(
                                            &mut ctx,
                                            src,
                                            &resp.body,
                                            &path,
                                            &budget,
                                            &mut outcome,
                                        );
                                    }
                                    None => outcome.modules_skipped += 1,
                                }
                            }
                        }
                    }
                    None => {
                        let name = format!("inline-module#{}", i + 1);
                        run_module(
                            &mut ctx,
                            &name,
                            inline.as_bytes(),
                            page_url,
                            &budget,
                            &mut outcome,
                        );
                    }
                }
                if outcome.panicked {
                    break;
                }
                run_jobs_into(&mut ctx, &budget, &mut outcome);
            }
            continue;
        }
        match src {
            Some(src) => match externals.iter().find(|(k, _)| k == src) {
                Some((_, Some(body))) => {
                    let body = String::from_utf8_lossy(body);
                    run_script(&mut ctx, src, body.as_bytes(), &budget, &mut outcome);
                }
                _ => outcome.errors.push(format!("{src}: not fetched")),
            },
            None => {
                let name = format!("inline#{}", i + 1);
                run_script(&mut ctx, &name, inline.as_bytes(), &budget, &mut outcome);
            }
        }
        if outcome.panicked {
            // Engine bug: what the page built so far still renders.
            break;
        }
        run_jobs_into(&mut ctx, &budget, &mut outcome);
    }

    if !outcome.panicked {
        // Lifecycle: DOMContentLoaded, settle, then load.
        run_script(
            &mut ctx,
            "DOMContentLoaded",
            b"__trust.readyState = \"interactive\"; __trust.fire(document, \"DOMContentLoaded\", true);",
            &budget,
            &mut outcome,
        );
        settle(&mut ctx, &budget, MAX_TICKS, &mut outcome);
        run_script(
            &mut ctx,
            "load",
            b"__trust.readyState = \"complete\"; __trust.fire(window, \"load\", false);",
            &budget,
            &mut outcome,
        );
        run_jobs_into(&mut ctx, &budget, &mut outcome);
    }

    drain_js_side(&mut ctx, &mut outcome);
    drain_rejections(&hooks, &mut outcome);
    outcome.fetches = ctx
        .realm()
        .host_defined()
        .get::<PageNet>()
        .map_or(0, |n| n.fetched.get());
    Ok(LoadedPage {
        ctx,
        dom,
        budget,
        outcome,
        started,
        page_url: parsed_url,
        hooks,
    })
}

/// Drain due timers and microtasks until quiet, budget-bounded.
/// Job errors (exceptions escaping microtasks — e.g. an async component
/// update throwing) are REAL page errors: collect, don't discard.
fn settle(ctx: &mut Context, budget: &Budget, max_ticks: usize, outcome: &mut Outcome) {
    let mut ticks = 0;
    loop {
        run_jobs_into(ctx, budget, outcome);
        if budget.exhausted() || ticks >= max_ticks {
            break;
        }
        match ctx.eval(Source::from_bytes(b"__trust.tick(1000)")) {
            Ok(v) if v.to_boolean() => ticks += 1,
            _ => break,
        }
    }
}

/// Drive the job queue to quiescence. When the page has a net grant its
/// async fetch jobs must be polled under the tokio runtime (so their
/// socket I/O is reactored) and they run CONCURRENTLY — this is where
/// `Promise.all([fetch, fetch])` actually overlaps. Bounded by the wall
/// deadline: on expiry the future is dropped at an await boundary and we
/// keep whatever the DOM already holds. No-net pages take the plain
/// synchronous path (no runtime needed).
fn run_jobs_into(ctx: &mut Context, budget: &Budget, outcome: &mut Outcome) {
    let handle = ctx
        .realm()
        .host_defined()
        .get::<PageNet>()
        .map(|n| n.handle.clone());
    let exec = ctx.downcast_job_executor::<PageJobExecutor>();
    let result = match (handle, exec) {
        (Some(handle), Some(exec)) => {
            let remaining = budget.remaining();
            if remaining.is_zero() {
                return;
            }
            let cell = RefCell::new(ctx);
            handle.block_on(async {
                tokio::time::timeout(remaining, exec.run_jobs_async(&cell))
                    .await
                    .unwrap_or(Ok(())) // deadline reached: render what we have
            })
        }
        _ => ctx.run_jobs(),
    };
    if let Err(err) = result {
        outcome.errors.push(format!("async job: {err}"));
    }
}

/// Report rejections that never found a handler. They land in the
/// console diagnostics channel, not errors: Boa never retracts
/// already-handled rejections (no Handle op observed), so pre-rejected
/// promises a page catches later would otherwise false-positive the
/// JS:n! badge. Diagnostics tools (js_diag/net_diag/canaries) print
/// console, which is where these earn their keep.
fn drain_rejections(hooks: &PageHooks, outcome: &mut Outcome) {
    for (_, reason) in hooks.rejections.borrow_mut().drain(..) {
        let line = format!("unhandled rejection: {reason}");
        if !outcome.console.contains(&line) {
            outcome.console.push(line);
        }
    }
}

/// Pull handler errors and console output collected on the JS side.
fn drain_js_side(ctx: &mut Context, outcome: &mut Outcome) {
    if let Ok(v) = ctx.eval(Source::from_bytes(
        b"__trust.errors.splice(0).join(\"\\u0000\")",
    )) && let Ok(s) = v.to_string(ctx)
    {
        let joined = s.to_std_string_lossy();
        outcome.errors.extend(
            joined
                .split('\0')
                .filter(|e| !e.is_empty())
                .map(String::from),
        );
    }
    if let Ok(v) = ctx.eval(Source::from_bytes(
        b"__trust.logs.splice(0).join(\"\\u0000\")",
    )) && let Ok(s) = v.to_string(ctx)
    {
        let joined = s.to_std_string_lossy();
        outcome.console.extend(
            joined
                .split('\0')
                .filter(|e| !e.is_empty())
                .map(String::from),
        );
    }
}

/// Run a page's scripts against a real DOM and return the post-JS HTML
/// (one-shot: the engine is dropped). Never fails: any error lands in
/// the Outcome and the best available document is returned.
pub fn transform(html: &str, env: &PageEnv) -> (String, Outcome) {
    match load_page(html, env) {
        Err(outcome) => (html.to_string(), outcome),
        Ok(mut page) => {
            page.outcome.elapsed = page.started.elapsed();
            let out = page.dom.borrow().serialize(DOCUMENT);
            (out, std::mem::take(&mut page.outcome))
        }
    }
}

// ---- The living page actor --------------------------------------------
//
// A displayed page keeps its engine + arena on a resident thread; the
// app talks to it over channels exactly like a telnet connection task.
// One live engine ever (the foreground page); navigation away drops the
// handle and the actor exits.

/// App → page.
#[derive(Debug)]
pub enum PageCmd {
    /// Dispatch a click on this arena node.
    Click(usize),
}

/// Page → app.
#[derive(Debug)]
pub enum PageEvt {
    /// A render of a page that stays alive for interaction.
    Updated { html: String, outcome: Outcome },
    /// Final render: nothing to interact with, the actor has exited.
    /// (Free efficiency: text articles never hold an engine.)
    Static { html: String, outcome: Outcome },
    /// An un-prevented click on a live anchor: the app navigates
    /// (absolute URL, already resolved against the page).
    Navigate(String),
    /// A dispatch produced errors but no content change.
    Trouble(Vec<String>),
}

#[derive(Debug)]
pub struct PageHandle {
    pub cmds: tokio::sync::mpsc::Sender<PageCmd>,
}

/// Wall budget for a single user-event dispatch (wire time extends it).
const DISPATCH_BUDGET: Duration = Duration::from_secs(1);
/// Settle ticks allowed after a dispatch (load-time settle uses MAX_TICKS).
const DISPATCH_TICKS: usize = 50;
/// The page thread owns all parsing/execution: same wide stack as the
/// one-shot path (Boa's parser recursion, see CLAUDE.md).
const PAGE_STACK: usize = 64 * 1024 * 1024;

/// Spawn a living page. The first event is either `Static` (actor
/// already gone) or `Updated` (alive, send it `PageCmd`s). Dropping the
/// handle shuts the actor down.
pub fn spawn_page(
    html: String,
    env: PageEnv,
) -> (PageHandle, tokio::sync::mpsc::Receiver<PageEvt>) {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
    let (evt_tx, evt_rx) = tokio::sync::mpsc::channel(16);
    let spawned = std::thread::Builder::new()
        .name(String::from("trust-page"))
        .stack_size(PAGE_STACK)
        .spawn(move || page_actor(html, env, cmd_rx, evt_tx));
    if spawned.is_err() {
        // The dropped evt sender tells the caller the page is gone.
    }
    (PageHandle { cmds: cmd_tx }, evt_rx)
}

fn page_actor(
    html: String,
    env: PageEnv,
    mut cmds: tokio::sync::mpsc::Receiver<PageCmd>,
    evts: tokio::sync::mpsc::Sender<PageEvt>,
) {
    let mut page = match load_page(&html, &env) {
        Ok(page) => page,
        Err(outcome) => {
            let _ = evts.blocking_send(PageEvt::Static { html, outcome });
            return;
        }
    };
    page.outcome.elapsed = page.started.elapsed();

    let (out, has_clickables) = extract_live(&mut page);
    let outcome = std::mem::take(&mut page.outcome);
    if !has_clickables {
        let _ = evts.blocking_send(PageEvt::Static { html: out, outcome });
        return;
    }
    if evts
        .blocking_send(PageEvt::Updated { html: out, outcome })
        .is_err()
    {
        return;
    }

    // The dispatch loop: blocked (zero CPU) until the app speaks or
    // drops the handle. Timers are frozen at rest by design — they only
    // advance inside a dispatch.
    while let Some(cmd) = cmds.blocking_recv() {
        match cmd {
            PageCmd::Click(node) => {
                let nav = dispatch_click_in(&mut page, node);
                drain_js_side(&mut page.ctx, &mut page.outcome);
                if let Some(url) = nav {
                    if evts.blocking_send(PageEvt::Navigate(url)).is_err() {
                        return;
                    }
                    continue; // app decides; we stay alive until dropped
                }
                if page.outcome.panicked {
                    // Engine bug: degrade to static, last render stands.
                    let _ = evts
                        .blocking_send(PageEvt::Trouble(std::mem::take(&mut page.outcome.errors)));
                    return;
                }
                let dirty = page.dom.borrow_mut().take_dirty();
                if dirty {
                    let (out, _) = extract_live(&mut page);
                    let outcome = std::mem::take(&mut page.outcome);
                    if evts
                        .blocking_send(PageEvt::Updated { html: out, outcome })
                        .is_err()
                    {
                        return;
                    }
                } else if !page.outcome.errors.is_empty()
                    && evts
                        .blocking_send(PageEvt::Trouble(std::mem::take(&mut page.outcome.errors)))
                        .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// Serialize for interaction: gather clickables (inherent tags + the
/// prelude's listener registry), mark live anchors, and skip wrapping
/// delegation containers (an element whose subtree holds other
/// interactives is a listener host, not a button).
fn extract_live(page: &mut LoadedPage) -> (String, bool) {
    use std::collections::HashSet;

    // Listener-bearing nodes, straight from the registry we own.
    let mut listeners: HashSet<usize> = HashSet::new();
    if let Ok(v) = page
        .ctx
        .eval(Source::from_bytes(b"__trust.clickables().join(\",\")"))
        && let Ok(s) = v.to_string(&mut page.ctx)
    {
        for part in s.to_std_string_lossy().split(',') {
            if let Ok(id) = part.parse::<usize>() {
                listeners.insert(id);
            }
        }
    }

    let dom = page.dom.borrow();
    // The COMPOSED tree: shadow content is where component UIs live.
    let everyone = dom.composed_descendants(crate::dom::DOCUMENT);
    let inherent: Vec<usize> = everyone
        .iter()
        .copied()
        .filter(|&d| {
            matches!(dom.tag_name(d), Some("button" | "summary"))
                || dom.attr(d, "onclick").is_some()
                || dom.attr(d, "role") == Some("button")
        })
        .collect();
    // Anchors are "live" (clicks route through the actor) when they or
    // a composed ancestor listen — which is how delegation works.
    let listens = |id: usize| -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if listeners.contains(&c) || dom.attr(c, "onclick").is_some() {
                return true;
            }
            cur = dom.parent_composed(c);
        }
        false
    };
    let mut anchors: Vec<usize> = Vec::new();
    for &d in &everyone {
        if dom.tag_name(d) == Some("a") && listens(d) {
            anchors.push(d);
        }
    }

    // Candidate buttons: inherent + listener elements, minus structure
    // tags and minus delegation containers (anything with another
    // interactive below it).
    let mut candidates: HashSet<usize> = inherent.into_iter().collect();
    candidates.extend(listeners.iter().copied());
    candidates.retain(|&c| !matches!(dom.tag_name(c), None | Some("html" | "body")));
    let mut containers: HashSet<usize> = HashSet::new();
    let interactive: Vec<usize> = candidates
        .iter()
        .copied()
        .chain(anchors.iter().copied())
        .collect();
    for &i in &interactive {
        let mut cur = dom.parent_composed(i);
        while let Some(p) = cur {
            containers.insert(p);
            cur = dom.parent_composed(p);
        }
    }
    let mut clickable: HashSet<usize> = candidates.difference(&containers).copied().collect();
    clickable.extend(anchors);

    let has_any = !clickable.is_empty();
    let html = dom.serialize_live(crate::dom::DOCUMENT, &clickable);
    drop(dom);
    // Extraction itself is not a page mutation.
    let _ = page.dom.borrow_mut().take_dirty();
    (html, has_any)
}

/// Dispatch a click into the live DOM under a fresh per-dispatch
/// budget. Returns the resolved navigation URL when an un-prevented
/// click landed on (or bubbled from inside) an anchor with an href.
fn dispatch_click_in(page: &mut LoadedPage, node: usize) -> Option<String> {
    page.budget.rearm(DISPATCH_BUDGET);
    if let Some(net) = page.ctx.realm().host_defined().get::<PageNet>() {
        net.fetched.set(0);
    }
    let _ = page.dom.borrow_mut().take_dirty();

    let call = format!("__trust.click({node})");
    let prevented = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        page.ctx.eval(Source::from_bytes(call.as_bytes()))
    })) {
        Ok(Ok(v)) => v.to_boolean(),
        Ok(Err(err)) => {
            page.outcome.errors.push(format!("click: {err}"));
            false
        }
        Err(_) => {
            page.outcome.errors.push(String::from(
                "click: engine panic (Boa bug) — page JS halted",
            ));
            page.outcome.panicked = true;
            return None;
        }
    };
    let mut dispatch_outcome = Outcome::default();
    settle(
        &mut page.ctx,
        &page.budget,
        DISPATCH_TICKS,
        &mut dispatch_outcome,
    );
    page.outcome.errors.extend(dispatch_outcome.errors);

    if prevented {
        return None;
    }
    let href = {
        let d = page.dom.borrow();
        let mut cur = Some(node);
        let mut found = None;
        while let Some(c) = cur {
            if d.tag_name(c) == Some("a")
                && let Some(h) = d.attr(c, "href")
            {
                found = Some(h.to_string());
                break;
            }
            cur = d.parent_composed(c);
        }
        found
    }?;
    let href = href.trim();
    // Fragments and javascript: pseudo-links never navigate.
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return None;
    }
    page.page_url
        .as_ref()
        .and_then(|base| base.join(href).ok())
        .map(|u| u.to_string())
}

/// Collect external script srcs (raw attribute values, document order)
/// so the fetch pipeline can resolve and download them first.
pub fn external_scripts(html: &str) -> Vec<String> {
    Dom::parse_document(html)
        .scripts()
        .into_iter()
        .filter(|(_, _, t)| is_classic(t))
        .filter_map(|(src, _, _)| src)
        .collect()
}

/// Does this page have scripts worth running at all?
pub fn has_scripts(html: &str) -> bool {
    !Dom::parse_document(html).scripts().is_empty()
}

/// Collect external stylesheet hrefs (raw attribute values, document
/// order) so the fetch pipeline can download them for the cascade.
pub fn external_stylesheets(html: &str) -> Vec<String> {
    Dom::parse_document(html).stylesheet_links()
}

/// The module graph the page announces up front:
/// `<link rel=modulepreload href>` and `<script type=module src>`.
/// The fetch pipeline downloads these in parallel and seeds the module
/// loader, so the graph stops serializing over the wire.
pub fn module_preloads(html: &str) -> Vec<String> {
    let dom = Dom::parse_document(html);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for id in dom.descendants(crate::dom::DOCUMENT) {
        let target = match dom.tag_name(id) {
            Some("link")
                if dom.attr(id, "rel").is_some_and(|r| {
                    r.split_ascii_whitespace()
                        .any(|w| w.eq_ignore_ascii_case("modulepreload"))
                }) =>
            {
                dom.attr(id, "href")
            }
            Some("script") if dom.attr(id, "type").is_some_and(|t| t.trim() == "module") => {
                dom.attr(id, "src")
            }
            _ => None,
        };
        if let Some(t) = target
            && seen.insert(t.to_string())
        {
            out.push(t.to_string());
        }
    }
    out
}

fn esc_js(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' | '\\' => vec!['\\', c],
            '\n' | '\r' => vec![],
            c if c.is_control() => vec![],
            c => vec![c],
        })
        .collect()
}

/// The web platform, self-hosted: built on the `__dom_*` syscalls. This
/// is plain portable JavaScript — an engine swap reuses it verbatim.
/// Phase 1 surface: DOM core, events (bubble phase), timers (virtual
/// time), classList/dataset/style, URL/URLSearchParams, atob/btoa, and
/// RAM-only page-lifetime storage. Deliberately absent: fetch/XHR (the
/// Phase 1 security envelope is zero I/O), ES modules, MutationObserver.
const PRELUDE: &str = r##"
(function () {
    "use strict";
    const g = globalThis;
    const cfg = g.__trust_cfg || { url: "about:blank", ua: "TRust/0.1", width: 640, height: 384 };
    const trust = { errors: [], logs: [], readyState: "loading" };
    g.__trust = trust;

    // --- node wrappers, identity-cached so wrap(id) === wrap(id) ---
    const W = new Map();
    function wrap(id) {
        if (id === null || id === undefined) return null;
        let w = W.get(id);
        if (w) return w;
        const t = __dom_node_type(id);
        w = t === 9 ? new Document(id)
            : t === 1 ? new Element(id)
            : t === 3 ? new Text(id)
            : t === 11 ? new DocumentFragment(id)
            : new Node(id);
        W.set(id, w);
        return w;
    }

    // The document base URL: <base href> when present (archive.org sets
    // one; SPA routers resolve '.' against it), the page URL otherwise.
    function baseHref() {
        const b = g.document.querySelector("base[href]");
        if (!b) return g.location.href;
        const u = __url_parse(b.getAttribute("href") || "", g.location.href);
        return u ? u[0] : g.location.href;
    }

    // --- events: listener registry + synchronous bubble dispatch ---
    const LS = new Map();
    function lsFor(target, type) {
        let m = LS.get(target);
        if (!m) { m = new Map(); LS.set(target, m); }
        let l = m.get(type);
        if (!l) { l = []; m.set(type, l); }
        return l;
    }
    class Event {
        constructor(type, opts) {
            this.type = String(type);
            this.bubbles = !!(opts && opts.bubbles);
            this.cancelable = !!(opts && opts.cancelable);
            this.defaultPrevented = false;
            this.target = null;
            this.currentTarget = null;
            this.isTrusted = false;
            this.detail = opts && opts.detail;
            this.timeStamp = 0;
        }
        preventDefault() { this.defaultPrevented = true; }
        stopPropagation() { this.__stop = true; }
        stopImmediatePropagation() { this.__stop = this.__stopNow = true; }
        // The same walk dispatch() bubbles along: shadow hop via __host.
        composedPath() {
            if (!this.target) return [];
            const path = [this.target];
            let p = this.target instanceof Node ? (this.target.parentNode || this.target.__host) : null;
            while (p) { path.push(p); p = p.parentNode || p.__host; }
            if (this.target !== g) path.push(g);
            return path;
        }
    }
    // on<event> attributes compile lazily at first dispatch and re-only
    // when the attribute text changes (zero cost at page load). Old-web
    // semantics: a handler returning false prevents the default.
    function attrHandler(cur, type) {
        if (!(cur instanceof Element)) return null;
        const src = cur.getAttribute("on" + type);
        if (src === null) return null;
        const cache = cur.__onCache || (cur.__onCache = {});
        const slot = cache[type];
        if (!slot || slot.src !== src) {
            let fn = null;
            try { fn = new Function("event", src); }
            catch (e) { trust.errors.push("on" + type + " compile: " + ((e && e.message) || e)); }
            cache[type] = { src: src, fn: fn };
            return fn;
        }
        return slot.fn;
    }
    function dispatch(target, ev, forceBubble) {
        ev.target = target;
        const path = [target];
        if (forceBubble || ev.bubbles) {
            let p = target instanceof Node ? (target.parentNode || target.__host) : null;
            while (p) { path.push(p); p = p.parentNode || p.__host; }
            if (target !== g) path.push(g);
        }
        for (const cur of path) {
            ev.currentTarget = cur;
            const af = attrHandler(cur, ev.type);
            if (af) {
                try { if (af.call(cur, ev) === false) ev.preventDefault(); }
                catch (e) { trust.errors.push("on" + ev.type + ": " + ((e && e.message) || e)); }
            }
            for (const fn of lsFor(cur, ev.type).slice()) {
                try {
                    if (typeof fn === "function") fn.call(cur, ev);
                    else fn.handleEvent(ev);
                }
                catch (e) { trust.errors.push(ev.type + " handler: " + ((e && e.message) || e)); }
                if (ev.__stopNow) break;
            }
            if (ev.__stop) break;
        }
        return !ev.defaultPrevented;
    }
    trust.fire = function (target, type, bubble) {
        dispatch(target, new Event(type), bubble);
    };
    // The actor's entry points: dispatch a user click; enumerate nodes
    // with click listeners (delegation hosts included — the actor sorts
    // containers from buttons).
    trust.click = function (id) {
        const t = wrap(id);
        if (!t) return false;
        const ev = new Event("click", { bubbles: true, cancelable: true });
        dispatch(t, ev, false);
        return ev.defaultPrevented;
    };
    trust.clickables = function () {
        const out = [];
        for (const entry of LS) {
            const target = entry[0], m = entry[1];
            if (target instanceof Node && typeof target.__id === "number") {
                const l = m.get("click");
                if (l && l.length) out.push(target.__id);
            }
        }
        return out;
    };

    // --- the DOM classes over the syscall boundary ---
    // Custom-element upgrades return the element being upgraded from
    // the base constructor (the standard polyfill trick), so
    // `class X extends HTMLElement { constructor(){ super(); ... } }`
    // initializes the EXISTING wrapper.
    const CE = { defs: new Map(), tags: new Map(), waiting: new Map(), upgrading: null };
    class Node {
        constructor(id) {
            if (CE.upgrading !== null) {
                const target = CE.upgrading;
                CE.upgrading = null;
                return target;
            }
            if (id === undefined && CE.tags.size) {
                // `new MyElement()` on a registered class: the platform
                // creates the element (routers mount pages this way).
                let c = new.target;
                while (c) {
                    const tag = CE.tags.get(c);
                    if (tag) {
                        this.__id = __dom_create_element(tag);
                        this.__ceUpgraded = true;
                        W.set(this.__id, this);
                        return;
                    }
                    c = Object.getPrototypeOf(c);
                }
            }
            this.__id = id;
        }
        get nodeType() { return __dom_node_type(this.__id); }
        get nodeName() {
            const t = __dom_tag(this.__id);
            if (t) return t.toUpperCase();
            const n = this.nodeType;
            return n === 3 ? "#text" : n === 9 ? "#document" : n === 8 ? "#comment" : n === 11 ? "#document-fragment" : "#node";
        }
        get parentNode() { return wrap(__dom_parent(this.__id)); }
        get parentElement() { const p = this.parentNode; return p && p.nodeType === 1 ? p : null; }
        get childNodes() { return __dom_children(this.__id).map(wrap); }
        get children() { return this.childNodes.filter((n) => n.nodeType === 1); }
        get firstChild() { const c = __dom_children(this.__id); return c.length ? wrap(c[0]) : null; }
        get lastChild() { const c = __dom_children(this.__id); return c.length ? wrap(c[c.length - 1]) : null; }
        get firstElementChild() { return this.children[0] || null; }
        get lastElementChild() { const c = this.children; return c[c.length - 1] || null; }
        get nextSibling() { return wrap(__dom_next(this.__id)); }
        get previousSibling() { return wrap(__dom_prev(this.__id)); }
        get nextElementSibling() { let s = this.nextSibling; while (s && s.nodeType !== 1) s = s.nextSibling; return s; }
        get previousElementSibling() { let s = this.previousSibling; while (s && s.nodeType !== 1) s = s.previousSibling; return s; }
        get textContent() { return __dom_text(this.__id); }
        set textContent(v) { __dom_set_text(this.__id, v === null || v === undefined ? "" : String(v)); }
        get nodeValue() { const t = this.nodeType; return t === 3 || t === 8 ? __dom_text(this.__id) : null; }
        set nodeValue(v) { const t = this.nodeType; if (t === 3 || t === 8) __dom_set_text(this.__id, String(v)); }
        get data() { return this.nodeValue ?? ""; }
        set data(v) { this.nodeValue = String(v); }
        get ownerDocument() { return g.document; }
        get isConnected() {
            let n = this;
            for (;;) {
                const p = n.parentNode;
                if (p) { n = p; continue; }
                if (n === g.document) return true;
                if (n.__host) { n = n.__host; continue; } // shadow hop
                return false;
            }
        }
        getRootNode() { let n = this; while (n.parentNode) n = n.parentNode; return n; }
        appendChild(c) {
            if (c && c.nodeType === 11 && !c.__host) { for (const k of c.childNodes) this.appendChild(k); return c; }
            __dom_append(this.__id, c.__id);
            if (CE.defs.size) ceScan(c);
            return c;
        }
        insertBefore(c, ref) {
            if (c && c.nodeType === 11 && !c.__host) { for (const k of c.childNodes) this.insertBefore(k, ref); return c; }
            __dom_insert_before(this.__id, c.__id, ref ? ref.__id : null);
            if (CE.defs.size) ceScan(c);
            return c;
        }
        removeChild(c) { if (CE.defs.size) ceDisconnect(c); __dom_detach(c.__id); return c; }
        replaceChild(n, old) {
            if (CE.defs.size) ceDisconnect(old);
            __dom_insert_before(this.__id, n.__id, old.__id);
            __dom_detach(old.__id);
            if (CE.defs.size) ceScan(n);
            return old;
        }
        remove() { if (CE.defs.size) ceDisconnect(this); __dom_detach(this.__id); }
        append(...ns) { for (const n of ns) this.appendChild(n && typeof n === "object" ? n : g.document.createTextNode(String(n))); }
        prepend(...ns) { const f = this.firstChild; for (const n of ns) this.insertBefore(n && typeof n === "object" ? n : g.document.createTextNode(String(n)), f); }
        // The ChildNode mixin: lit's svg templates go through
        // replaceWith in the Template constructor.
        before(...ns) { const p = this.parentNode; if (!p) return; for (const n of ns) p.insertBefore(n && typeof n === "object" ? n : g.document.createTextNode(String(n)), this); }
        after(...ns) { const p = this.parentNode; if (!p) return; const r = this.nextSibling; for (const n of ns) p.insertBefore(n && typeof n === "object" ? n : g.document.createTextNode(String(n)), r); }
        replaceWith(...ns) { this.before(...ns); this.remove(); }
        replaceChildren(...ns) { let c; while ((c = this.firstChild)) this.removeChild(c); this.append(...ns); }
        cloneNode(deep) { return wrap(__dom_clone(this.__id, !!deep)); }
        contains(o) { while (o) { if (o === this) return true; o = o.parentNode; } return false; }
        hasChildNodes() { return __dom_children(this.__id).length > 0; }
        compareDocumentPosition() { return 0; }
        normalize() {}
        addEventListener(type, fn) {
            // Functions AND `{ handleEvent }` objects (Lit's EventParts
            // register themselves as listeners).
            if (typeof fn === "function" || (fn && typeof fn.handleEvent === "function")) {
                const l = lsFor(this, String(type));
                if (!l.includes(fn)) l.push(fn);
            }
        }
        removeEventListener(type, fn) { const l = lsFor(this, String(type)); const i = l.indexOf(fn); if (i >= 0) l.splice(i, 1); }
        dispatchEvent(ev) { return dispatch(this, ev, false); }
        querySelector(s) { const r = __dom_query(this.__id, String(s), true); return r.length ? wrap(r[0]) : null; }
        querySelectorAll(s) { return __dom_query(this.__id, String(s), false).map(wrap); }
        getElementsByTagName(t) { return this.querySelectorAll(String(t)); }
        getElementsByClassName(c) { return this.querySelectorAll(String(c).trim().split(/\s+/).map((x) => "." + x).join("")); }
    }
    Node.ELEMENT_NODE = 1; Node.TEXT_NODE = 3; Node.COMMENT_NODE = 8;
    Node.DOCUMENT_NODE = 9; Node.DOCUMENT_FRAGMENT_NODE = 11;

    function makeStyle() {
        return {
            cssText: "",
            setProperty(k, v) { this[k] = String(v); },
            getPropertyValue(k) { return typeof this[k] === "string" ? this[k] : ""; },
            removeProperty(k) { const v = this[k]; delete this[k]; return typeof v === "string" ? v : ""; },
        };
    }
    const kebab = (s) => s.replace(/[A-Z]/g, (m) => "-" + m.toLowerCase());
    // el.style is backed by the REAL style attribute: writes are DOM
    // mutations (dirty bit, serialized, visibility honored), reads
    // parse the attribute. That's how show/hide actually works here.
    function styleFor(el) {
        const parse = () => {
            const m = {};
            const raw = el.getAttribute("style") || "";
            for (const part of raw.split(";")) {
                const i = part.indexOf(":");
                if (i > 0) m[part.slice(0, i).trim().toLowerCase()] = part.slice(i + 1).trim();
            }
            return m;
        };
        const write = (m) => {
            const keys = Object.keys(m);
            if (keys.length) el.setAttribute("style", keys.map((k) => k + ": " + m[k]).join("; "));
            else el.removeAttribute("style");
        };
        return new Proxy({}, {
            get(_, p) {
                if (typeof p !== "string") return undefined;
                if (p === "cssText") return el.getAttribute("style") || "";
                if (p === "setProperty") return (k, v) => { const m = parse(); m[String(k).toLowerCase()] = String(v); write(m); };
                if (p === "getPropertyValue") return (k) => parse()[String(k).toLowerCase()] || "";
                if (p === "removeProperty") return (k) => { const m = parse(); const key = String(k).toLowerCase(); const v = m[key] || ""; delete m[key]; write(m); return v; };
                if (p === "length") return Object.keys(parse()).length;
                return parse()[kebab(p)] ?? "";
            },
            set(_, p, v) {
                if (typeof p !== "string") return true;
                if (p === "cssText") {
                    if (String(v).trim()) el.setAttribute("style", String(v));
                    else el.removeAttribute("style");
                    return true;
                }
                const m = parse();
                const key = kebab(p);
                if (v === "" || v === null || v === undefined) delete m[key];
                else m[key] = String(v);
                write(m);
                return true;
            },
            has() { return true; },
            deleteProperty(_, p) {
                if (typeof p === "string") { const m = parse(); delete m[kebab(p)]; write(m); }
                return true;
            },
        });
    }

    class Element extends Node {
        get tagName() { return (__dom_tag(this.__id) || "").toUpperCase(); }
        get localName() { return __dom_tag(this.__id) || ""; }
        getAttribute(n) { return __dom_get_attr(this.__id, String(n)); }
        setAttribute(n, v) {
            n = String(n); v = String(v);
            const old = this.__ceUpgraded ? this.getAttribute(n) : null;
            __dom_set_attr(this.__id, n, v);
            ceAttrChanged(this, n.toLowerCase(), old, v);
        }
        setAttributeNS(_, n, v) { this.setAttribute(n, v); }
        removeAttribute(n) {
            n = String(n);
            const old = this.__ceUpgraded ? this.getAttribute(n) : null;
            __dom_remove_attr(this.__id, n);
            ceAttrChanged(this, n.toLowerCase(), old, null);
        }
        hasAttribute(n) { return __dom_get_attr(this.__id, String(n)) !== null; }
        getAttributeNames() { return __dom_attr_names(this.__id); }
        hasAttributes() { return __dom_attr_names(this.__id).length > 0; }
        // Lit's ?attr= boolean bindings commit through this.
        toggleAttribute(name, force) {
            const want = force === undefined ? !this.hasAttribute(name) : !!force;
            if (want) this.setAttribute(name, "");
            else this.removeAttribute(name);
            return want;
        }
        get id() { return this.getAttribute("id") || ""; }
        set id(v) { this.setAttribute("id", v); }
        get className() { return this.getAttribute("class") || ""; }
        set className(v) { this.setAttribute("class", v); }
        get name() { return this.getAttribute("name") || ""; }
        set name(v) { this.setAttribute("name", v); }
        get value() { const v = this.getAttribute("value"); return v === null ? "" : v; }
        set value(v) { this.setAttribute("value", String(v)); }
        get checked() { return this.hasAttribute("checked"); }
        set checked(v) { if (v) this.setAttribute("checked", ""); else this.removeAttribute("checked"); }
        get disabled() { return this.hasAttribute("disabled"); }
        set disabled(v) { if (v) this.setAttribute("disabled", ""); else this.removeAttribute("disabled"); }
        get hidden() { return this.hasAttribute("hidden"); }
        set hidden(v) { if (v) this.setAttribute("hidden", ""); else this.removeAttribute("hidden"); }
        get type() { return this.getAttribute("type") || ""; }
        set type(v) { this.setAttribute("type", String(v)); }
        get href() { const r = this.getAttribute("href"); if (r === null) return ""; const u = __url_parse(r, baseHref()); return u ? u[0] : r; }
        set href(v) { this.setAttribute("href", String(v)); }
        // Anchor URL components (the create-an-<a>-to-parse-URLs trick;
        // router-slot reads m.pathname). Empty-string fallbacks, no-op
        // setters (strict mode throws on getter-only assignment).
        __urlPart(i) {
            if (this.localName !== "a" && this.localName !== "area") return undefined;
            const u = __url_parse(this.getAttribute("href") || "", baseHref());
            return u ? u[i] : "";
        }
        get protocol() { return this.__urlPart(1); } set protocol(v) {}
        get host() { return this.__urlPart(2); } set host(v) {}
        get hostname() { return this.__urlPart(3); } set hostname(v) {}
        get port() { return this.__urlPart(4); } set port(v) {}
        get pathname() { return this.__urlPart(5); } set pathname(v) {}
        get search() { return this.__urlPart(6); } set search(v) {}
        get hash() { return this.__urlPart(7); } set hash(v) {}
        get origin() { return this.__urlPart(8); }
        get src() { const r = this.getAttribute("src"); if (r === null) return ""; const u = __url_parse(r, baseHref()); return u ? u[0] : r; }
        set src(v) { this.setAttribute("src", String(v)); }
        get innerHTML() { return __dom_inner_html(this.__id); }
        set innerHTML(v) {
            __dom_set_inner_html(this.__id, String(v));
            if (CE.defs.size) ceScan(this);
        }
        get content() {
            // <template>.content: the inert fragment its markup parses into.
            return this.localName === "template" ? wrap(__dom_template_content(this.__id)) : undefined;
        }
        attachShadow(init) {
            const id = __dom_attach_shadow(this.__id);
            let sr = W.get(id);
            if (!(sr instanceof ShadowRoot)) {
                sr = new ShadowRoot(id);
                W.set(id, sr);
            }
            sr.__host = this;
            sr.__mode = init && init.mode === "closed" ? "closed" : "open";
            this.__sr = sr;
            return sr;
        }
        get shadowRoot() { return this.__sr && this.__sr.__mode === "open" ? this.__sr : null; }
        // ElementInternals, minimally: form components construct with
        // this unguarded (archive.org's dropdowns) — always-valid,
        // form-less internals keep them booting.
        attachInternals() {
            return {
                form: null, shadowRoot: this.__sr || null, willValidate: false,
                validity: { valid: true }, validationMessage: "", labels: [],
                states: new Set(), ariaLabel: null,
                setFormValue() {}, setValidity() {},
                checkValidity() { return true; }, reportValidity() { return true; },
            };
        }
        get outerHTML() { return __dom_outer_html(this.__id); }
        get innerText() { return this.textContent; }
        set innerText(v) { this.textContent = v; }
        insertAdjacentHTML(p, h) {
            __dom_insert_adjacent(this.__id, String(p).toLowerCase(), String(h));
            if (CE.defs.size) { const par = this.parentNode; ceScan(par || this); }
        }
        insertAdjacentElement(p, el) {
            const pos = String(p).toLowerCase();
            if (pos === "beforeend") this.appendChild(el);
            else if (pos === "afterbegin") this.insertBefore(el, this.firstChild);
            else if (pos === "beforebegin" && this.parentNode) this.parentNode.insertBefore(el, this);
            else if (pos === "afterend" && this.parentNode) this.parentNode.insertBefore(el, this.nextSibling);
            return el;
        }
        get style() { if (!this.__style) this.__style = styleFor(this); return this.__style; }
        get dataset() {
            if (!this.__ds) {
                const el = this;
                this.__ds = new Proxy({}, {
                    get(_, p) { return typeof p === "string" ? (el.getAttribute("data-" + kebab(p)) ?? undefined) : undefined; },
                    set(_, p, v) { if (typeof p === "string") el.setAttribute("data-" + kebab(p), String(v)); return true; },
                    has(_, p) { return typeof p === "string" && el.getAttribute("data-" + kebab(p)) !== null; },
                });
            }
            return this.__ds;
        }
        get classList() {
            if (!this.__cl) {
                const el = this;
                const get = () => (el.getAttribute("class") || "").split(/\s+/).filter(Boolean);
                const set = (l) => el.setAttribute("class", l.join(" "));
                this.__cl = {
                    add(...cs) { const l = get(); for (const c of cs) if (!l.includes(String(c))) l.push(String(c)); set(l); },
                    remove(...cs) { const ss = cs.map(String); set(get().filter((x) => !ss.includes(x))); },
                    toggle(c, force) {
                        const has = get().includes(String(c));
                        const want = force === undefined ? !has : !!force;
                        if (want && !has) this.add(c);
                        if (!want && has) this.remove(c);
                        return want;
                    },
                    contains(c) { return get().includes(String(c)); },
                    item(i) { return get()[i] ?? null; },
                    get length() { return get().length; },
                    toString() { return el.getAttribute("class") || ""; },
                };
            }
            return this.__cl;
        }
        matches(s) { return !!__dom_matches(this.__id, String(s)); }
        webkitMatchesSelector(s) { return this.matches(s); }
        closest(s) { let e = this; while (e && e.nodeType === 1) { if (e.matches(s)) return e; e = e.parentNode; } return null; }
        click() {} focus() {} blur() {} scrollIntoView() {}
        get offsetWidth() { return 0; }
        get offsetHeight() { return 0; }
        get offsetParent() { return null; }
        get clientWidth() { return 0; }
        get clientHeight() { return 0; }
        getBoundingClientRect() { return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 }; }
        getClientRects() { return []; }
    }

    class Text extends Node {
        get data() { return __dom_text(this.__id); }
        set data(v) { __dom_set_text(this.__id, String(v)); }
        get length() { return this.data.length; }
    }

    class Document extends Node {
        get documentElement() { return wrap(__dom_doc_element()); }
        get body() { return this.querySelector("body"); }
        get head() { return this.querySelector("head"); }
        get readyState() { return trust.readyState; }
        get title() { const t = this.querySelector("title"); return t ? t.textContent : ""; }
        set title(v) { const t = this.querySelector("title"); if (t) t.textContent = String(v); }
        get cookie() { return __cookie_get(); }
        set cookie(v) { __cookie_set(String(v)); }
        get location() { return g.location; }
        get defaultView() { return g; }
        get documentURI() { return g.location.href; }
        get URL() { return g.location.href; }
        get currentScript() { return null; }
        get implementation() {
            const doc = this;
            return {
                createHTMLDocument() {
                    // A detached mini-document, real enough for jQuery's
                    // support checks and parseHTML: same arena, same API.
                    const html = doc.createElement("html");
                    const head = doc.createElement("head");
                    const body = doc.createElement("body");
                    html.appendChild(head); html.appendChild(body);
                    return {
                        documentElement: html, head: head, body: body,
                        createElement: (t) => doc.createElement(t),
                        createTextNode: (s) => doc.createTextNode(s),
                        createDocumentFragment: () => doc.createDocumentFragment(),
                        getElementsByTagName: (t) => html.getElementsByTagName(t),
                        querySelector: (s) => html.querySelector(s),
                        querySelectorAll: (s) => html.querySelectorAll(s),
                    };
                },
            };
        }
        get forms() { return this.querySelectorAll("form"); }
        get links() { return this.querySelectorAll("a[href]"); }
        get images() { return this.querySelectorAll("img"); }
        get scripts() { return []; }
        createElement(t) {
            const el = wrap(__dom_create_element(String(t)));
            const ctor = CE.defs.get(String(t).toLowerCase());
            if (ctor) upgradeElement(el, ctor);
            return el;
        }
        get adoptedStyleSheets() { return this.__adopted || (this.__adopted = []); }
        set adoptedStyleSheets(v) { this.__adopted = v; adoptedSync(this); }
        createElementNS(_, t) { return this.createElement(t); }
        createTextNode(s) { return wrap(__dom_create_text(s === undefined ? "" : String(s))); }
        createComment(s) { return wrap(__dom_create_comment(s === undefined ? "" : String(s))); }
        importNode(n, deep) { return n.cloneNode(!!deep); }
        createTreeWalker(root, whatToShow) { return new TreeWalker(root, whatToShow); }
        createDocumentFragment() { return wrap(__dom_create_fragment()); }
        getElementById(i) { return wrap(__dom_get_by_id(String(i))); }
        getElementsByName(n) { return this.querySelectorAll("[name=" + String(n) + "]"); }
        createEvent() { return new Event(""); }
        hasFocus() { return true; }
        write(s) { const host = this.body || this.documentElement; if (host) host.insertAdjacentHTML("beforeend", String(s)); }
        writeln(s) { this.write(s + "\n"); }
        open() {} close() {}
    }

    class DocumentFragment extends Node {}
    class Comment extends Node {}
    // Lit walks comment markers with one of these.
    class TreeWalker {
        constructor(root, whatToShow) {
            this.root = root;
            this.currentNode = root;
            this.whatToShow = (whatToShow === undefined ? 0xFFFFFFFF : whatToShow) >>> 0;
        }
        __shows(n) {
            const bit = n.nodeType === 1 ? 1 : n.nodeType === 3 ? 4 : n.nodeType === 8 ? 128 : 0;
            return (this.whatToShow & bit) !== 0;
        }
        nextNode() {
            let n = this.currentNode;
            for (;;) {
                let next = n.firstChild;
                if (!next) {
                    let cur = n;
                    while (!next && cur && cur !== this.root) {
                        next = cur.nextSibling;
                        cur = cur.parentNode;
                    }
                }
                if (!next) return null;
                n = next;
                if (this.__shows(n)) {
                    this.currentNode = n;
                    return n;
                }
            }
        }
    }
    class ShadowRoot extends Node {
        get host() { return this.__host || null; }
        get mode() { return this.__mode || "open"; }
        get innerHTML() { return __dom_inner_html(this.__id); }
        set innerHTML(v) {
            __dom_set_inner_html(this.__id, String(v));
            if (CE.defs.size) ceScan(this);
        }
        get adoptedStyleSheets() { return this.__adopted || (this.__adopted = []); }
        set adoptedStyleSheets(v) { this.__adopted = v; adoptedSync(this); }
        getElementById(i) { const r = this.querySelectorAll("[id]"); for (const e of r) if (e.id === String(i)) return e; return null; }
    }

    // --- the custom elements registry ---
    function upgradeElement(el, ctor) {
        if (el.__ceUpgraded) return;
        el.__ceUpgraded = true;
        // Read observedAttributes BEFORE constructing — the platform
        // contract define() relies on. Lit's static getter runs its
        // finalize() here, creating reactive accessors; construct
        // first and instance fields shadow them forever.
        let observed = [];
        try { observed = ctor.observedAttributes || []; } catch (e) { observed = []; }
        Object.setPrototypeOf(el, ctor.prototype);
        CE.upgrading = el;
        try { new ctor(); }
        catch (e) { trust.errors.push("custom element ctor: " + ((e && e.message) || e)); }
        finally { CE.upgrading = null; }
        for (const a of observed) {
            const v = el.getAttribute(a);
            if (v !== null && typeof el.attributeChangedCallback === "function") {
                try { el.attributeChangedCallback(a, null, v); }
                catch (e) { trust.errors.push("attributeChangedCallback: " + ((e && e.message) || e)); }
            }
        }
        maybeConnect(el);
    }
    function maybeConnect(el) {
        if (el.__ceUpgraded && !el.__ceConnected && el.isConnected
            && typeof el.connectedCallback === "function") {
            el.__ceConnected = true;
            try { el.connectedCallback(); }
            catch (e) { trust.errors.push("connectedCallback: " + ((e && e.message) || e)); }
        }
    }
    function ceScan(node) {
        if (!node || typeof node !== "object") return;
        if (node instanceof Element) {
            const ctor = CE.defs.get(node.localName);
            if (ctor) { upgradeElement(node, ctor); maybeConnect(node); }
            if (node.__sr) ceScan(node.__sr);
        }
        if (node.childNodes) for (const c of node.childNodes) ceScan(c);
    }
    function ceDisconnect(node) {
        if (!node || typeof node !== "object") return;
        if (node.__ceConnected && typeof node.disconnectedCallback === "function") {
            node.__ceConnected = false;
            try { node.disconnectedCallback(); }
            catch (e) { trust.errors.push("disconnectedCallback: " + ((e && e.message) || e)); }
        }
        if (node.childNodes) for (const c of node.childNodes) ceDisconnect(c);
    }
    function ceAttrChanged(el, name, old, val) {
        if (!el.__ceUpgraded || old === val) return;
        const observed = (el.constructor && el.constructor.observedAttributes) || [];
        if (observed.includes(name) && typeof el.attributeChangedCallback === "function") {
            try { el.attributeChangedCallback(name, old, val); }
            catch (e) { trust.errors.push("attributeChangedCallback: " + ((e && e.message) || e)); }
        }
    }
    const customElements = {
        define(name, ctor) {
            name = String(name).toLowerCase();
            if (CE.defs.has(name)) return;
            // The registration-time observedAttributes read (see above).
            try { void (ctor.observedAttributes || []); } catch (e) { /* page's problem */ }
            CE.defs.set(name, ctor);
            CE.tags.set(ctor, name);
            for (const el of g.document.querySelectorAll(name)) {
                upgradeElement(el, ctor);
            }
            const w = CE.waiting.get(name);
            if (w) { CE.waiting.delete(name); w.resolve(ctor); }
        },
        get(name) { return CE.defs.get(String(name).toLowerCase()); },
        getName(ctor) { return CE.tags.get(ctor) || null; },
        whenDefined(name) {
            name = String(name).toLowerCase();
            if (CE.defs.has(name)) return Promise.resolve(CE.defs.get(name));
            let w = CE.waiting.get(name);
            if (!w) {
                w = {};
                w.promise = new Promise((resolve) => { w.resolve = resolve; });
                CE.waiting.set(name, w);
            }
            return w.promise;
        },
        upgrade(root) { if (CE.defs.size) ceScan(root); },
    };
    // Scopes (document / shadow roots) that ever adopted sheets, so a
    // later replaceSync() re-pushes their joined text to the cascade.
    const adoptedScopes = [];
    const adoptedSync = (scope) => {
        if (!adoptedScopes.includes(scope)) adoptedScopes.push(scope);
        let text = "";
        for (const s of scope.__adopted || []) text += ((s && s.__text) || "") + "\n";
        __dom_adopt_styles(scope.__id, text);
    };
    const sheetSync = (sheet) => {
        for (const scope of adoptedScopes) {
            if ((scope.__adopted || []).includes(sheet)) adoptedSync(scope);
        }
    };
    class CSSStyleSheet {
        constructor() { this.cssRules = []; this.__text = ""; }
        replace(t) { this.replaceSync(t); return Promise.resolve(this); }
        replaceSync(t) { this.__text = String(t); sheetSync(this); }
        insertRule(r) { this.__text += "\n" + String(r); sheetSync(this); return 0; }
        deleteRule() {}
    }

    // Distinct subclasses so `instanceof` answers honestly (false for
    // our wrappers): Vue picks SVG namespaces by SVGElement checks.
    class SVGElement extends Element {}
    class HTMLInputElement extends Element {}
    class HTMLSelectElement extends Element {}
    class HTMLTextAreaElement extends Element {}
    class HTMLFormElement extends Element {}
    class HTMLAnchorElement extends Element {}
    class HTMLImageElement extends Element {}
    class HTMLScriptElement extends Element {}
    class HTMLButtonElement extends Element {}

    g.Node = Node; g.Element = Element; g.HTMLElement = Element;
    g.Text = Text; g.Document = Document; g.HTMLDocument = Document;
    g.DocumentFragment = DocumentFragment; g.Comment = Comment;
    g.Event = Event; g.CustomEvent = Event;
    g.ShadowRoot = ShadowRoot;
    g.TreeWalker = TreeWalker;
    g.NodeFilter = {
        SHOW_ALL: 0xFFFFFFFF, SHOW_ELEMENT: 1, SHOW_TEXT: 4, SHOW_COMMENT: 128,
        FILTER_ACCEPT: 1, FILTER_REJECT: 2, FILTER_SKIP: 3,
    };
    g.CSSStyleSheet = CSSStyleSheet;
    g.customElements = customElements;
    g.SVGElement = SVGElement;
    g.HTMLInputElement = HTMLInputElement; g.HTMLSelectElement = HTMLSelectElement;
    g.HTMLTextAreaElement = HTMLTextAreaElement; g.HTMLFormElement = HTMLFormElement;
    g.HTMLAnchorElement = HTMLAnchorElement; g.HTMLImageElement = HTMLImageElement;
    g.HTMLScriptElement = HTMLScriptElement; g.HTMLButtonElement = HTMLButtonElement;
    g.Image = class { constructor() { return g.document.createElement("img"); } };

    g.window = g; g.self = g; g.top = g; g.parent = g;
    g.document = wrap(0);

    // --- environment ---
    const L = __url_parse(cfg.url, null) || [cfg.url, "", "", "", "", "", "", "", ""];
    g.location = {
        href: L[0], protocol: L[1], host: L[2], hostname: L[3], port: L[4],
        pathname: L[5], search: L[6], hash: L[7], origin: L[8],
        assign() {}, replace() {}, reload() {},
        toString() { return this.href; },
    };
    g.navigator = {
        userAgent: cfg.ua, language: "en", languages: ["en"],
        platform: "Linux", cookieEnabled: true, onLine: true,
        plugins: [], mimeTypes: [], webdriver: false,
    };
    g.screen = { width: cfg.width, height: cfg.height, availWidth: cfg.width, availHeight: cfg.height, colorDepth: 24, pixelDepth: 24 };
    g.innerWidth = cfg.width; g.innerHeight = cfg.height;
    g.outerWidth = cfg.width; g.outerHeight = cfg.height;
    g.devicePixelRatio = 1; g.pageXOffset = 0; g.pageYOffset = 0;
    g.scrollX = 0; g.scrollY = 0;
    // History real enough for SPA routers: state round-trips and the
    // URL arguments land in location (router-slot writes state then
    // destructures history.state back — a null there kills routing).
    const updateLoc = (u) => {
        if (u === undefined || u === null) return;
        const p = __url_parse(String(u), g.location.href);
        if (!p) return;
        g.location.href = p[0]; g.location.protocol = p[1]; g.location.host = p[2];
        g.location.hostname = p[3]; g.location.port = p[4]; g.location.pathname = p[5];
        g.location.search = p[6]; g.location.hash = p[7]; g.location.origin = p[8];
    };
    g.history = {
        length: 1, state: null, scrollRestoration: "auto",
        pushState(s, _t, u) { this.state = s === undefined ? null : s; this.length += 1; updateLoc(u); },
        replaceState(s, _t, u) { this.state = s === undefined ? null : s; updateLoc(u); },
        back() {}, forward() {}, go() {},
    };
    g.getComputedStyle = (el) => (el instanceof Element ? el.style : makeStyle());
    g.matchMedia = (m) => ({ matches: false, media: String(m), addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {} });
    g.alert = () => {}; g.confirm = () => false; g.prompt = () => null;
    g.scroll = g.scrollTo = g.scrollBy = () => {};
    g.getSelection = () => ({ toString: () => "", rangeCount: 0, removeAllRanges() {} });
    g.MutationObserver = class { observe() {} disconnect() {} takeRecords() { return []; } };
    // No viewport here, so everything observed intersects, once,
    // asynchronously — infinite scrollers and lazy tiles render their
    // content instead of waiting for a scroll that can't happen.
    g.IntersectionObserver = class {
        constructor(cb) { this.__cb = cb; this.__dead = false; }
        observe(el) {
            g.setTimeout(() => {
                if (this.__dead) return;
                const r = { top: 0, left: 0, bottom: 1, right: 1, width: 1, height: 1 };
                try {
                    this.__cb([{ isIntersecting: true, intersectionRatio: 1, target: el,
                        time: 0, boundingClientRect: r, intersectionRect: r, rootBounds: r }], this);
                } catch (e) { trust.errors.push("IntersectionObserver: " + ((e && e.message) || e)); }
            }, 0);
        }
        unobserve() {} disconnect() { this.__dead = true; } takeRecords() { return []; }
    };
    g.ResizeObserver = class { observe() {} unobserve() {} disconnect() {} };
    g.requestIdleCallback = (fn) => g.setTimeout(() => fn({ didTimeout: false, timeRemaining: () => 0 }), 0);
    g.cancelIdleCallback = (id) => g.clearTimeout(id);
    g.addEventListener = (t, f) => {
        if (typeof f === "function" || (f && typeof f.handleEvent === "function")) {
            const l = lsFor(g, String(t));
            if (!l.includes(f)) l.push(f);
        }
    };
    g.removeEventListener = (t, f) => { const l = lsFor(g, String(t)); const i = l.indexOf(f); if (i >= 0) l.splice(i, 1); };
    g.dispatchEvent = (ev) => dispatch(g, ev, false);
    g.performance = { now: () => 0, timing: {}, mark() {}, measure() {}, getEntriesByType: () => [] };

    // RAM-only, session-lifetime storage: origin-bucketed maps shared
    // across pages, dead with the process, never disk.
    function makeStorage(kind) {
        return {
            getItem: (k) => __storage_get(kind, String(k)),
            setItem: (k, v) => { __storage_set(kind, String(k), String(v)); },
            removeItem: (k) => { __storage_remove(kind, String(k)); },
            clear: () => { __storage_clear(kind); },
            key: (i) => __storage_key(kind, Number(i)),
            get length() { return __storage_len(kind); },
        };
    }
    g.localStorage = makeStorage("local");
    g.sessionStorage = makeStorage("session");

    // --- timers on virtual time, driven by the Rust settle loop ---
    const timers = { q: [], now: 0, seq: 1 };
    g.setTimeout = (fn, d) => {
        if (typeof fn !== "function") return 0;
        const id = timers.seq++;
        timers.q.push({ id, at: timers.now + Math.max(0, Number(d) || 0), fn, every: null });
        return id;
    };
    g.setInterval = (fn, d) => {
        if (typeof fn !== "function") return 0;
        const id = timers.seq++;
        const every = Math.max(4, Number(d) || 4);
        timers.q.push({ id, at: timers.now + every, fn, every });
        return id;
    };
    g.clearTimeout = g.clearInterval = (id) => { timers.q = timers.q.filter((t) => t.id !== id); };
    g.requestAnimationFrame = (fn) => g.setTimeout(() => fn(timers.now), 16);
    g.cancelAnimationFrame = g.clearTimeout;
    g.queueMicrotask = (fn) => { Promise.resolve().then(fn).catch((e) => trust.errors.push("microtask: " + ((e && e.message) || e))); };
    trust.tick = function (horizon) {
        let best = null;
        for (const t of timers.q) if (t.at <= horizon && (!best || t.at < best.at)) best = t;
        if (!best) return false;
        timers.q.splice(timers.q.indexOf(best), 1);
        timers.now = Math.max(timers.now, best.at);
        if (best.every !== null) timers.q.push({ id: best.id, at: timers.now + best.every, fn: best.fn, every: best.every });
        try { best.fn(); } catch (e) { trust.errors.push("timer: " + ((e && e.message) || e)); }
        return true;
    };

    // --- console into the outcome's ring ---
    const log = (level) => (...a) => {
        if (trust.logs.length < 100) trust.logs.push(level + ": " + a.map((x) => { try { return String(x); } catch { return "?"; } }).join(" "));
    };
    g.console = { log: log("log"), info: log("info"), warn: log("warn"), error: log("error"), debug: log("debug"), trace: log("trace"), dir: log("dir"), group() {}, groupEnd() {}, table: log("table"), time() {}, timeEnd() {}, count() {}, assert() {} };

    // --- small web APIs ---
    const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    g.btoa = (s) => {
        s = String(s); let out = "";
        for (let i = 0; i < s.length; i += 3) {
            const c1 = s.charCodeAt(i), c2 = s.charCodeAt(i + 1), c3 = s.charCodeAt(i + 2);
            if (c1 > 255 || c2 > 255 || c3 > 255) throw new Error("btoa: invalid character");
            out += B64[c1 >> 2] + B64[((c1 & 3) << 4) | (isNaN(c2) ? 0 : c2 >> 4)]
                + (isNaN(c2) ? "=" : B64[((c2 & 15) << 2) | (isNaN(c3) ? 0 : c3 >> 6)])
                + (isNaN(c3) ? "=" : B64[c3 & 63]);
        }
        return out;
    };
    g.atob = (s) => {
        s = String(s).replace(/=+$/, ""); let out = "", buf = 0, bits = 0;
        for (const ch of s) {
            const v = B64.indexOf(ch);
            if (v < 0) continue;
            buf = (buf << 6) | v; bits += 6;
            if (bits >= 8) { bits -= 8; out += String.fromCharCode((buf >> bits) & 255); }
        }
        return out;
    };
    g.TextEncoder = class TextEncoder {
        get encoding() { return "utf-8"; }
        encode(s) {
            const bytes = [];
            for (const ch of String(s === undefined ? "" : s)) {
                const c = ch.codePointAt(0);
                if (c < 0x80) bytes.push(c);
                else if (c < 0x800) bytes.push(0xc0 | (c >> 6), 0x80 | (c & 63));
                else if (c < 0x10000) bytes.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
                else bytes.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
            }
            return new Uint8Array(bytes);
        }
    };
    g.TextDecoder = class TextDecoder {
        constructor(label) {
            const l = String(label === undefined ? "utf-8" : label).toLowerCase();
            if (l !== "utf-8" && l !== "utf8" && l !== "unicode-1-1-utf-8") throw new RangeError("TextDecoder: only utf-8 is supported");
        }
        get encoding() { return "utf-8"; }
        decode(input) {
            if (input === undefined) return "";
            const b = input instanceof Uint8Array ? input
                : ArrayBuffer.isView(input) ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
                : new Uint8Array(input);
            let out = "", i = 0;
            while (i < b.length) {
                const x = b[i];
                let c, n;
                if (x < 0x80) { c = x; n = 0; }
                else if ((x & 0xe0) === 0xc0) { c = x & 31; n = 1; }
                else if ((x & 0xf0) === 0xe0) { c = x & 15; n = 2; }
                else if ((x & 0xf8) === 0xf0) { c = x & 7; n = 3; }
                else { out += "�"; i += 1; continue; }
                if (i + n >= b.length) { out += "�"; i += 1; continue; }
                let ok = true;
                for (let k = 1; k <= n; k++) {
                    if ((b[i + k] & 0xc0) !== 0x80) { ok = false; break; }
                    c = (c << 6) | (b[i + k] & 63);
                }
                if (!ok) { out += "�"; i += 1; continue; }
                out += String.fromCodePoint(c);
                i += n + 1;
            }
            return out;
        }
    };
    // --- Intl: an en-only prelude shim. Measured 2026-06-12: Boa's
    // bundled ICU costs +11MB and its DateTimeFormat/DisplayNames are
    // broken anyway. Honest-enough English output for a terminal;
    // resolvedOptions/supportedLocalesOf exist so feature-detection
    // passes and pages stop taking polyfill/error paths.
    {
        const localeList = (l) => (l === undefined ? [] : Array.isArray(l) ? Array.from(l) : [l]).map(String);
        const supEn = (l) => localeList(l).filter((s) => /^en($|-)/i.test(s));
        const grouped = (s) => {
            const i = s.indexOf(".");
            const head = i < 0 ? s : s.slice(0, i), tail = i < 0 ? "" : s.slice(i);
            return head.replace(/\B(?=(\d{3})+$)/g, ",") + tail;
        };
        const CURRENCY = { USD: "$", EUR: "€", GBP: "£", JPY: "¥" };
        class NumberFormat {
            constructor(locales, options) { this.__o = options || {}; }
            format(n) {
                const o = this.__o;
                n = Number(n);
                if (!isFinite(n)) return isNaN(n) ? "NaN" : n > 0 ? "∞" : "-∞";
                const neg = n < 0 || (n === 0 && 1 / n < 0);
                let v = Math.abs(n);
                if (o.style === "percent") v *= 100;
                let min = o.minimumFractionDigits, max = o.maximumFractionDigits;
                if (min === undefined) min = o.style === "currency" ? 2 : 0;
                if (max === undefined) max = Math.max(min, o.style === "currency" ? 2 : o.style === "percent" ? 0 : 3);
                let s = v.toFixed(Math.min(20, max));
                const dot = s.indexOf(".");
                if (max > min && dot >= 0) {
                    const h = s.slice(0, dot);
                    let f = s.slice(dot + 1).replace(/0+$/, "");
                    while (f.length < min) f += "0";
                    s = f ? h + "." + f : h;
                }
                if (o.useGrouping !== false) s = grouped(s);
                if (o.style === "currency") {
                    const c = String(o.currency || "USD").toUpperCase();
                    s = (CURRENCY[c] || c + " ") + s;
                }
                if (o.style === "percent") s += "%";
                return neg ? "-" + s : s;
            }
            formatToParts(n) { return [{ type: "literal", value: this.format(n) }]; }
            resolvedOptions() {
                const o = this.__o;
                return Object.assign({ locale: "en-US", numberingSystem: "latn", notation: "standard", style: "decimal", useGrouping: o.useGrouping === false ? false : "auto", minimumIntegerDigits: 1 }, o);
            }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        const p2 = (x) => String(x).padStart(2, "0");
        class DateTimeFormat {
            constructor(locales, options) { this.__o = options || {}; }
            format(d) {
                d = d === undefined ? new Date() : new Date(d);
                if (isNaN(d.getTime())) return "Invalid Date";
                const o = this.__o;
                const wantsDate = !!(o.year || o.month || o.day || o.weekday || o.dateStyle);
                const wantsTime = !!(o.hour || o.minute || o.second || o.timeStyle);
                const date = d.getFullYear() + "-" + p2(d.getMonth() + 1) + "-" + p2(d.getDate());
                const secs = o.second || o.timeStyle ? ":" + p2(d.getSeconds()) : "";
                const time = p2(d.getHours()) + ":" + p2(d.getMinutes()) + secs;
                if (wantsTime && !wantsDate) return time;
                if (wantsDate && wantsTime) return date + ", " + time;
                return date;
            }
            formatToParts(d) { return [{ type: "literal", value: this.format(d) }]; }
            resolvedOptions() { return Object.assign({ locale: "en-US", calendar: "gregory", numberingSystem: "latn", timeZone: "UTC" }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        class Collator {
            constructor(locales, options) {
                // `var`, NOT const: Boa 0.21 panics (define opcode, OOB
                // binding slot) when a closure capturing a block-scoped
                // constructor local is invoked from a native callback —
                // and `compare` exists to be handed to Array#sort.
                var o = this.__o = options || {};
                var fold = o.sensitivity === "base" || o.sensitivity === "accent"
                    ? (s) => String(s).toLowerCase() : (s) => String(s);
                this.compare = (a, b) => {
                    a = fold(a); b = fold(b);
                    var na = o.numeric ? parseFloat(a) : NaN;
                    var nb = o.numeric ? parseFloat(b) : NaN;
                    if (!isNaN(na) && !isNaN(nb) && na !== nb) return na < nb ? -1 : 1;
                    return a < b ? -1 : a > b ? 1 : 0;
                };
            }
            resolvedOptions() { return Object.assign({ locale: "en-US", usage: "sort", sensitivity: "variant", numeric: false }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        class DisplayNames {
            constructor(locales, options) { this.__o = options || {}; }
            of(code) { return String(code); }
            resolvedOptions() { return Object.assign({ locale: "en-US", style: "long", fallback: "code" }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        class PluralRules {
            constructor(locales, options) { this.__o = options || {}; }
            select(n) { return Number(n) === 1 ? "one" : "other"; }
            resolvedOptions() { return Object.assign({ locale: "en-US", type: "cardinal", pluralCategories: ["one", "other"] }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        class RelativeTimeFormat {
            constructor(locales, options) { this.__o = options || {}; }
            format(v, unit) {
                v = Number(v);
                unit = String(unit).replace(/s$/, "");
                const n = Math.abs(v), u = n === 1 ? unit : unit + "s";
                return v < 0 ? n + " " + u + " ago" : "in " + n + " " + u;
            }
            formatToParts(v, unit) { return [{ type: "literal", value: this.format(v, unit) }]; }
            resolvedOptions() { return Object.assign({ locale: "en-US", numeric: "always", style: "long" }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        g.Intl = {
            NumberFormat, DateTimeFormat, Collator, DisplayNames, PluralRules, RelativeTimeFormat,
            getCanonicalLocales: localeList,
        };
        Number.prototype.toLocaleString = function (locales, options) { return new NumberFormat(locales, options).format(this); };
        Date.prototype.toLocaleDateString = function () { return new DateTimeFormat(0, { year: "numeric", month: "numeric", day: "numeric" }).format(this); };
        Date.prototype.toLocaleTimeString = function () { return new DateTimeFormat(0, { hour: "numeric", minute: "numeric", second: "numeric" }).format(this); };
        Date.prototype.toLocaleString = function () { return new DateTimeFormat(0, { year: "numeric", hour: "numeric", second: "numeric" }).format(this); };
    }
    const dec = (s) => { try { return decodeURIComponent(String(s).replace(/\+/g, " ")); } catch { return String(s); } };
    class URLSearchParams {
        constructor(init) {
            this.__p = [];
            if (typeof init === "string") {
                for (const kv of init.replace(/^\?/, "").split("&")) {
                    if (!kv) continue;
                    const i = kv.indexOf("=");
                    this.__p.push(i < 0 ? [dec(kv), ""] : [dec(kv.slice(0, i)), dec(kv.slice(i + 1))]);
                }
            } else if (init && typeof init === "object") {
                for (const k of Object.keys(init)) this.__p.push([String(k), String(init[k])]);
            }
        }
        get(k) { const e = this.__p.find((p) => p[0] === String(k)); return e ? e[1] : null; }
        getAll(k) { return this.__p.filter((p) => p[0] === String(k)).map((p) => p[1]); }
        has(k) { return this.__p.some((p) => p[0] === String(k)); }
        set(k, v) { this.delete(k); this.__p.push([String(k), String(v)]); }
        append(k, v) { this.__p.push([String(k), String(v)]); }
        delete(k) { this.__p = this.__p.filter((p) => p[0] !== String(k)); }
        forEach(fn) { for (const [k, v] of this.__p.slice()) fn(v, k, this); }
        keys() { return this.__p.map((p) => p[0])[Symbol.iterator](); }
        values() { return this.__p.map((p) => p[1])[Symbol.iterator](); }
        entries() { return this.__p.slice()[Symbol.iterator](); }
        [Symbol.iterator]() { return this.entries(); }
        toString() { return this.__p.map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v)).join("&"); }
    }
    class URL {
        constructor(href, base) {
            const r = __url_parse(String(href), base === undefined || base === null ? null : String(base));
            if (!r) throw new TypeError("Invalid URL: " + href);
            this.href = r[0]; this.protocol = r[1]; this.host = r[2]; this.hostname = r[3];
            this.port = r[4]; this.pathname = r[5]; this.search = r[6]; this.hash = r[7]; this.origin = r[8];
        }
        get searchParams() { return new URLSearchParams(this.search); }
        toString() { return this.href; }
        toJSON() { return this.href; }
    }
    g.URLSearchParams = URLSearchParams;
    g.URL = URL;

    // --- the network, over the __http_fetch_async syscall ---
    // Requests fire as async jobs; the JS thread does NOT block on them,
    // so many in-flight fetches overlap and Promise.all runs them in
    // parallel. The promise settles when the bytes arrive. Only legacy
    // synchronous XHR still blocks (via the __http_fetch syscall).
    class Headers {
        constructor(init) {
            this.__h = {};
            if (init) {
                if (Array.isArray(init)) { for (const kv of init) this.__h[String(kv[0]).toLowerCase()] = String(kv[1]); }
                else if (init.__h) { Object.assign(this.__h, init.__h); }
                else if (typeof init === "object") { for (const k of Object.keys(init)) this.__h[String(k).toLowerCase()] = String(init[k]); }
            }
        }
        get(k) { const v = this.__h[String(k).toLowerCase()]; return v === undefined ? null : v; }
        set(k, v) { this.__h[String(k).toLowerCase()] = String(v); }
        append(k, v) { this.set(k, v); }
        has(k) { return String(k).toLowerCase() in this.__h; }
        delete(k) { delete this.__h[String(k).toLowerCase()]; }
        forEach(fn) { for (const k of Object.keys(this.__h)) fn(this.__h[k], k, this); }
    }
    g.Headers = Headers;
    g.AbortController = class AbortController {
        constructor() { this.signal = { aborted: false, reason: undefined, onabort: null, addEventListener() {}, removeEventListener() {} }; }
        abort() { this.signal.aborted = true; }
    };

    g.fetch = function (input, init) {
        try {
            const url = input && typeof input === "object" && input.url !== undefined ? String(input.url) : String(input);
            const method = String((init && init.method) || (input && input.method) || "GET").toUpperCase();
            let body = init && init.body !== undefined && init.body !== null ? init.body : null;
            if (body !== null && typeof body !== "string") {
                if (body instanceof URLSearchParams) body = body.toString();
                else return Promise.reject(new TypeError("fetch: unsupported body type"));
            }
            const ctype = new Headers(init && init.headers).get("content-type")
                || (body !== null ? "text/plain;charset=UTF-8" : null);
            return __http_fetch_async(url, method, body, ctype).then(function (r) {
                if (!r) throw new TypeError("fetch failed or blocked: " + url);
                const status = r[0], respCType = r[1], text = r[2];
                return {
                    ok: status >= 200 && status < 300,
                    status: status,
                    statusText: "",
                    url: url,
                    redirected: false,
                    type: "basic",
                    bodyUsed: false,
                    headers: new Headers({ "content-type": respCType }),
                    text() { return Promise.resolve(text); },
                    json() { try { return Promise.resolve(JSON.parse(text)); } catch (e) { return Promise.reject(e); } },
                    clone() { return this; },
                    arrayBuffer() { return Promise.reject(new TypeError("arrayBuffer unsupported")); },
                    blob() { return Promise.reject(new TypeError("blob unsupported")); },
                    formData() { return Promise.reject(new TypeError("formData unsupported")); },
                };
            });
        } catch (e) { return Promise.reject(e); }
    };

    class XMLHttpRequest {
        constructor() {
            this.readyState = 0; this.status = 0; this.statusText = "";
            this.responseText = ""; this.response = ""; this.responseType = "";
            this.responseURL = ""; this.timeout = 0; this.withCredentials = false;
            this.__h = {}; this.__ls = {};
        }
        open(method, url, isAsync) {
            this.__method = String(method).toUpperCase();
            this.__url = String(url);
            this.__sync = isAsync === false;
            this.readyState = 1;
            this.__fire("readystatechange");
        }
        setRequestHeader(k, v) { this.__h[String(k).toLowerCase()] = String(v); }
        getResponseHeader(k) { return String(k).toLowerCase() === "content-type" ? (this.__ctype || null) : null; }
        getAllResponseHeaders() { return this.__ctype ? "content-type: " + this.__ctype + "\r\n" : ""; }
        overrideMimeType() {}
        abort() {}
        addEventListener(t, f) { (this.__ls[t] = this.__ls[t] || []).push(f); }
        removeEventListener(t, f) { const l = this.__ls[t] || []; const i = l.indexOf(f); if (i >= 0) l.splice(i, 1); }
        __fire(t) {
            const ev = new Event(t); ev.target = this;
            const on = this["on" + t];
            if (typeof on === "function") { try { on.call(this, ev); } catch (e) { trust.errors.push("xhr on" + t + ": " + ((e && e.message) || e)); } }
            for (const f of (this.__ls[t] || []).slice()) { try { f.call(this, ev); } catch (e) { trust.errors.push("xhr " + t + ": " + ((e && e.message) || e)); } }
        }
        __finish(r) {
            if (!r) {
                this.readyState = 4; this.status = 0;
                this.__fire("readystatechange"); this.__fire("error"); this.__fire("loadend");
                return;
            }
            this.status = r[0]; this.__ctype = r[1];
            this.responseText = r[2]; this.responseURL = this.__url;
            if (this.responseType === "json") { try { this.response = JSON.parse(r[2]); } catch (e) { this.response = null; } }
            else { this.response = r[2]; }
            this.readyState = 4;
            this.__fire("readystatechange"); this.__fire("load"); this.__fire("loadend");
        }
        send(body) {
            const b = body === undefined || body === null ? null : String(body);
            const ctype = this.__h["content-type"] || (b !== null ? "text/plain;charset=UTF-8" : null);
            if (this.__sync) {
                this.__finish(__http_fetch(this.__url, this.__method || "GET", b, ctype));
            } else {
                // The request runs concurrently, but its callbacks are
                // macrotasks (not microtasks): defer __finish into the
                // timer queue so promise reactions still run first, as on
                // the real platform.
                const xhr = this;
                __http_fetch_async(this.__url, this.__method || "GET", b, ctype)
                    .then(function (r) { g.setTimeout(function () { xhr.__finish(r); }, 0); });
            }
        }
    }
    g.XMLHttpRequest = XMLHttpRequest;
})();
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_run_and_globals_persist_within_a_page() {
        let mut ctx = page_context();
        let budget = Budget::new(WALL_BUDGET);
        let mut outcome = Outcome::default();
        run_script(&mut ctx, "a", b"globalThis.x = 6;", &budget, &mut outcome);
        run_script(
            &mut ctx,
            "b",
            b"globalThis.y = x * 7;",
            &budget,
            &mut outcome,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let y = ctx
            .eval(Source::from_bytes(b"y"))
            .unwrap()
            .to_number(&mut ctx)
            .unwrap();
        assert_eq!(y, 42.0);
    }

    #[test]
    fn a_throwing_script_is_tolerated_and_tagged() {
        let mut ctx = page_context();
        let budget = Budget::new(WALL_BUDGET);
        let mut outcome = Outcome::default();
        run_script(
            &mut ctx,
            "bad.js",
            b"throw new Error('boom');",
            &budget,
            &mut outcome,
        );
        run_script(
            &mut ctx,
            "good.js",
            b"globalThis.alive = true;",
            &budget,
            &mut outcome,
        );
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0].starts_with("bad.js:"),
            "{:?}",
            outcome.errors
        );
        assert!(outcome.notice().unwrap().starts_with("JS: bad.js:"));
        let alive = ctx.eval(Source::from_bytes(b"alive")).unwrap();
        assert!(alive.to_boolean());
    }

    #[test]
    fn a_loop_bomb_trips_the_iteration_limit() {
        let mut ctx = page_context();
        let budget = Budget::new(WALL_BUDGET);
        let mut outcome = Outcome::default();
        let started = Instant::now();
        run_script(
            &mut ctx,
            "bomb.js",
            b"while (true) {}",
            &budget,
            &mut outcome,
        );
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        // The limit must fire in seconds, not hang the suite.
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn an_exhausted_budget_skips_remaining_scripts() {
        let mut ctx = page_context();
        let budget = Budget::new(Duration::ZERO);
        let mut outcome = Outcome::default();
        run_script(
            &mut ctx,
            "late.js",
            b"globalThis.ran = true;",
            &budget,
            &mut outcome,
        );
        assert_eq!(outcome.errors.len(), 1);
        assert!(outcome.errors[0].contains("budget"), "{:?}", outcome.errors);
        assert!(
            ctx.eval(Source::from_bytes(b"globalThis.ran"))
                .unwrap()
                .is_undefined()
        );
    }

    #[test]
    fn annex_b_old_web_syntax_is_accepted() {
        // HTML-comment script guards and escape(): the jQuery-era web.
        let mut ctx = page_context();
        let budget = Budget::new(WALL_BUDGET);
        let mut outcome = Outcome::default();
        let src = b"<!--\nglobalThis.esc = escape('a b');\n//-->";
        run_script(&mut ctx, "oldweb.js", src, &budget, &mut outcome);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let esc = ctx.eval(Source::from_bytes(b"esc")).unwrap();
        assert_eq!(
            esc.to_string(&mut ctx).unwrap().to_std_string_lossy(),
            "a%20b"
        );
    }

    /// The acceptance canary: real framework bundles against the REAL
    /// arena DOM, through the full `transform` pipeline — no stubs.
    /// Each bundle loads as the page's external script, then a probe
    /// script drives it: jQuery manipulates the page, Vue mounts a
    /// component, D3 appends elements. Manual because it wants
    /// release-mode timings and local bundle files:
    ///
    /// ```sh
    /// mkdir -p target/canary && cd target/canary
    /// curl -LO https://code.jquery.com/jquery-3.7.1.min.js
    /// curl -LO https://unpkg.com/vue@3/dist/vue.global.prod.js
    /// curl -LO https://d3js.org/d3.v7.min.js
    /// cargo test --release canary -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "manual canary: needs bundles in target/canary/ and --release timings"]
    fn canary_real_world_bundles() {
        const PROBE: &str = r##"
            var msg = [];
            function out(s) { document.body.appendChild(document.createTextNode("\n" + s)); }
            if (typeof jQuery !== "undefined") {
                msg.push("jQuery " + jQuery.fn.jquery);
                jQuery("#target").text("jQuery drives the real DOM");
                jQuery("<p class='made-by-jq'>and builds new elements</p>").appendTo(document.body);
                jQuery("p.made-by-jq").addClass("found-again");
            }
            if (typeof Vue !== "undefined") {
                msg.push("Vue " + Vue.version);
                try {
                    Vue.createApp({
                        data: function () { return { who: "TRust" }; },
                        // A render function, as precompiled Vue apps ship.
                        // (Template strings need the in-browser compiler,
                        // whose `with` blocks trip a Boa VM bug.)
                        render: function () { return Vue.h("p", {}, "Vue renders inside " + this.who); },
                    }).mount("#target");
                } catch (e) { out("vue mount failed: " + (e && e.message)); }
            }
            if (typeof d3 !== "undefined") {
                msg.push("d3 " + d3.version);
                try {
                    d3.select("#target").append("p").attr("class", "made-by-d3").text("d3 appended this");
                } catch (e) { out("d3 select failed: " + (e && e.message)); }
            }
            out("BOOTED: " + (msg.join(", ") || "nothing"));
        "##;

        let dir = std::path::Path::new("target/canary");
        let mut entries: Vec<_> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "js"))
                .collect(),
            Err(_) => {
                eprintln!("no target/canary/ directory — see the doc comment for setup");
                return;
            }
        };
        entries.sort();
        assert!(
            !entries.is_empty(),
            "target/canary/ exists but holds no .js bundles"
        );

        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let source = std::fs::read(&path).unwrap();
            let kb = source.len() / 1024;
            // ES-module bundles (lit-core) need a module script tag; a
            // classic load chokes on `export`. Booting without errors is
            // the whole smoke here — lit_canary in http.rs drives Lit.
            let module = source
                .windows(7)
                .any(|w| w == b"export{" || w == b"import ");
            let tag = if module { " type=\"module\"" } else { "" };
            let html = format!(
                "<html><head><script{tag} src=\"/bundle.js\"></script></head>\
                 <body><div id=\"target\">static placeholder</div>\
                 <script>{PROBE}</script></body></html>"
            );
            let mut env = PageEnv::bare("https://example.com/");
            env.externals = vec![("/bundle.js".to_string(), Some(source))];
            let started = Instant::now();
            let (out, outcome) = transform(&html, &env);
            let booted = out
                .lines()
                .find(|l| l.contains("BOOTED:"))
                .unwrap_or("(no probe output)")
                .trim()
                .to_string();
            eprintln!(
                "{name} ({kb} KB): {:?} total ({:?} in scripts), {} — errors: {:?}",
                started.elapsed(),
                outcome.elapsed,
                booted,
                outcome.errors,
            );
            for marker in ["made-by-jq", "Vue renders inside TRust", "made-by-d3"] {
                if out.contains(marker) {
                    eprintln!("    proof in the rendered HTML: {marker}");
                }
            }
            for line in out.lines().filter(|l| l.contains("failed:")) {
                eprintln!("    {}", line.trim());
            }
            assert!(
                outcome.elapsed < COMPUTE_BUDGET,
                "{name} blew the page compute budget: {:?}",
                outcome.elapsed
            );
        }
    }

    // ---- Phase 1: scripts against the real DOM ----

    fn page(html: &str) -> (String, Outcome) {
        transform(html, &PageEnv::bare("https://example.com/a/page"))
    }

    /// Diagnostic: run an arbitrary local HTML file through the full
    /// transform and dump the result. `TRUST_JS_DIAG=/path/page.html
    /// cargo test js_diag -- --ignored --nocapture`
    #[test]
    #[ignore = "manual diagnostic, needs TRUST_JS_DIAG=<file>"]
    fn js_diag() {
        let Ok(path) = std::env::var("TRUST_JS_DIAG") else {
            eprintln!("set TRUST_JS_DIAG to an HTML file");
            return;
        };
        let html = std::fs::read_to_string(&path).unwrap();
        let (out, outcome) = transform(&html, &PageEnv::bare("https://example.com/"));
        eprintln!("=== outcome: {:?}", outcome);
        eprintln!("=== post-JS HTML ===\n{out}");
    }

    // ---- Phase 2b: the living page actor ----

    /// Spawn an actor with no net/storage and hand back the channels.
    fn live(html: &str) -> (PageHandle, tokio::sync::mpsc::Receiver<PageEvt>) {
        spawn_page(
            html.to_string(),
            PageEnv::bare("https://example.com/dir/page"),
        )
    }

    #[test]
    fn a_page_with_nothing_to_click_goes_static() {
        let (_handle, mut events) = live(
            "<body><p id=t>quiet</p><script>\
             document.getElementById('t').textContent = 'ran';</script></body>",
        );
        match events.blocking_recv() {
            Some(PageEvt::Static { html, outcome }) => {
                assert!(html.contains("ran"), "{html}");
                assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
            }
            other => panic!("expected Static, got {other:?}"),
        }
        // The actor exited: the event channel is closed.
        assert!(events.blocking_recv().is_none());
    }

    #[test]
    fn clicks_drive_an_onclick_counter() {
        let (handle, mut events) = live(
            "<body><h1 id=n>0</h1>\
             <button id=inc onclick=\"var n = document.getElementById('n'); \
             n.textContent = String(Number(n.textContent) + 1);\">+1</button>\
             <script>void 0;</script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected first Updated");
        };
        // The button is followable: wrapped in a click marker.
        let marker = html
            .split("x-trust-js:")
            .nth(1)
            .and_then(|rest| rest.split(':').next())
            .expect("marker in extraction")
            .parse::<usize>()
            .unwrap();

        handle.cmds.blocking_send(PageCmd::Click(marker)).unwrap();
        let Some(PageEvt::Updated { html, outcome }) = events.blocking_recv() else {
            panic!("expected Updated after click");
        };
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(html.contains(">1</h1>"), "{html}");

        handle.cmds.blocking_send(PageCmd::Click(marker)).unwrap();
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected Updated after second click");
        };
        assert!(html.contains(">2</h1>"), "{html}");
    }

    #[test]
    fn preloaded_modules_run_without_network() {
        // The parallel-prefetch contract: bodies seeded into
        // PageEnv.preloaded serve the module graph — entry AND imports
        // — with no net grant at all.
        let mut env = PageEnv::bare("https://example.com/");
        env.preloaded = vec![
            (
                String::from("https://example.com/main.js"),
                b"import { x } from './lib.js';\
                  document.getElementById('t').textContent = x;"
                    .to_vec(),
            ),
            (
                String::from("https://example.com/lib.js"),
                b"export const x = 'preloaded graph';".to_vec(),
            ),
        ];
        let (out, outcome) = transform(
            "<body><div id=t></div>\
             <script type=module src='/main.js'></script></body>",
            &env,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.modules_skipped, 0);
        assert!(out.contains("preloaded graph"), "{out}");
    }

    #[test]
    fn adopted_stylesheets_hide_through_the_cascade() {
        // Both adoption orders: replaceSync-then-adopt (Lit's shape)
        // and adopt-then-replaceSync (needs the sheet→scope re-sync).
        let (out, outcome) = page(
            r##"<body><div id=host></div><p class=light>light target</p>
            <p>stays</p><script>
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            root.innerHTML = '<p class="sec">shadow secret</p><p>shadow public</p>';
            const sheet = new CSSStyleSheet();
            sheet.replaceSync('.sec { display: none }');
            root.adoptedStyleSheets = [sheet];
            const docSheet = new CSSStyleSheet();
            document.adoptedStyleSheets = [docSheet];
            docSheet.replaceSync('.light { display: none }');
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(!out.contains("shadow secret"), "{out}");
        assert!(out.contains("shadow public"), "{out}");
        assert!(!out.contains("light target"), "{out}");
        assert!(out.contains("stays"), "{out}");
    }

    #[test]
    fn live_click_toggles_stylesheet_class_visibility() {
        // The CSS step-1 payoff on a living page: a click that flips a
        // class genuinely expands and collapses the panel.
        let (handle, mut events) = live(
            "<head><style>.panel{display:none}.panel.open{display:block}</style></head>\
             <body><button id=b onclick=\"document.getElementById('p')\
             .classList.toggle('open')\">toggle</button>\
             <div id=p class=panel>panel payload</div>\
             <script>void 0;</script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected first Updated");
        };
        assert!(!html.contains("panel payload"), "{html}");
        let marker = html
            .split("x-trust-js:")
            .nth(1)
            .and_then(|rest| rest.split(':').next())
            .expect("marker in extraction")
            .parse::<usize>()
            .unwrap();

        handle.cmds.blocking_send(PageCmd::Click(marker)).unwrap();
        let Some(PageEvt::Updated { html, outcome }) = events.blocking_recv() else {
            panic!("expected Updated after click");
        };
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(html.contains("panel payload"), "{html}");

        handle.cmds.blocking_send(PageCmd::Click(marker)).unwrap();
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected Updated after second click");
        };
        assert!(!html.contains("panel payload"), "{html}");
    }

    #[test]
    fn a_mutationless_click_emits_nothing() {
        let (handle, mut events) = live(
            "<body><h1 id=n>0</h1><div id=noop></div><div id=real></div><script>\
             document.getElementById('noop').addEventListener('click', function () {});\
             document.getElementById('real').addEventListener('click', function () {\
               document.getElementById('n').textContent = 'changed';\
             });</script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected first Updated");
        };
        let id_of = |html: &str, anchor_text: &str| -> usize {
            // Find the marker wrapping the element whose id attr matches.
            let at = html.find(anchor_text).unwrap();
            html[..at]
                .rfind("x-trust-js:")
                .map(|i| {
                    html[i + "x-trust-js:".len()..]
                        .split(':')
                        .next()
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                })
                .unwrap()
        };
        let noop = id_of(&html, "id=\"noop\"");
        let real = id_of(&html, "id=\"real\"");

        // Dispatch the no-op first, then the mutating click: the FIRST
        // event to arrive must already be the mutation (the no-op
        // dispatch emitted nothing — the dirty bit held).
        handle.cmds.blocking_send(PageCmd::Click(noop)).unwrap();
        handle.cmds.blocking_send(PageCmd::Click(real)).unwrap();
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected Updated from the mutating click");
        };
        assert!(html.contains("changed"), "{html}");
    }

    #[test]
    fn live_anchor_clicks_navigate_unless_prevented() {
        let (handle, mut events) = live(
            "<body><a id=go href=\"../next\">go</a>\
             <a id=stay href=\"/nowhere\">stay</a><script>\
             document.getElementById('go').addEventListener('click', function () {});\
             document.getElementById('stay').addEventListener('click', function (e) {\
               e.preventDefault();\
               document.body.appendChild(document.createTextNode('prevented!'));\
             });</script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected first Updated");
        };
        // Both anchors carry rewritten hrefs preserving the original.
        assert!(html.contains("x-trust-js:"), "{html}");
        assert!(html.contains(":../next"), "{html}");
        let id_for = |needle: &str| {
            let at = html.find(needle).unwrap();
            html[..at].rfind("x-trust-js:").map(|i| {
                html[i + "x-trust-js:".len()..]
                    .split(':')
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
        };
        let go = id_for(">go</a>").unwrap();
        let stay = id_for(">stay</a>").unwrap();

        // preventDefault: the page mutates instead of navigating.
        handle.cmds.blocking_send(PageCmd::Click(stay)).unwrap();
        match events.blocking_recv() {
            Some(PageEvt::Updated { html, .. }) => {
                assert!(html.contains("prevented!"), "{html}")
            }
            other => panic!("expected Updated, got {other:?}"),
        }

        // No prevention: resolved against the page URL.
        handle.cmds.blocking_send(PageCmd::Click(go)).unwrap();
        match events.blocking_recv() {
            Some(PageEvt::Navigate(url)) => {
                assert_eq!(url, "https://example.com/next");
            }
            other => panic!("expected Navigate, got {other:?}"),
        }
    }

    #[test]
    fn onclick_returning_false_prevents_navigation() {
        let (handle, mut events) = live(
            "<body><a id=go href=\"/away\" onclick=\"\
             document.body.appendChild(document.createTextNode('handled')); return false;\
             \">go</a><script>void 0;</script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected first Updated");
        };
        let id = html
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .unwrap()
            .parse::<usize>()
            .unwrap();
        handle.cmds.blocking_send(PageCmd::Click(id)).unwrap();
        match events.blocking_recv() {
            Some(PageEvt::Updated { html, .. }) => assert!(html.contains("handled"), "{html}"),
            other => panic!("expected Updated (no navigation), got {other:?}"),
        }
    }

    #[test]
    fn dropping_the_handle_ends_the_actor() {
        let (handle, mut events) =
            live("<body><button onclick=\"void 0\">x</button><script>void 0;</script></body>");
        assert!(matches!(
            events.blocking_recv(),
            Some(PageEvt::Updated { .. })
        ));
        drop(handle);
        assert!(events.blocking_recv().is_none());
    }

    #[test]
    fn style_display_toggling_shows_and_hides_content() {
        // Modeled on her expandtest.html: the canonical pure-JS
        // show/hide. style writes are real DOM mutations and the
        // serializer honors display:none.
        let (handle, mut events) = live(
            "<body><a href=\"#\" id=\"toggleLink\">show/hide</a>\
             <div id=\"hiddenLinks\">\
             <a href=\"#\">Additional Link 1</a>\
             <a href=\"#\">Additional Link 2</a>\
             </div><script>\
             document.addEventListener('DOMContentLoaded', () => {\
               const t = document.getElementById('toggleLink');\
               const h = document.getElementById('hiddenLinks');\
               h.style.display = 'none';\
               t.addEventListener('click', (e) => {\
                 e.preventDefault();\
                 h.style.display = h.style.display === 'none' ? 'block' : 'none';\
               });\
             });</script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected first Updated");
        };
        // The initial JS hide already took: links absent at first paint.
        assert!(!html.contains("Additional Link 1"), "{html}");
        let toggle = html
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .unwrap()
            .parse::<usize>()
            .unwrap();

        // Click: shown.
        handle.cmds.blocking_send(PageCmd::Click(toggle)).unwrap();
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected Updated after show");
        };
        assert!(html.contains("Additional Link 1"), "{html}");
        assert!(html.contains("Additional Link 2"), "{html}");

        // Click again: hidden, and no navigation ever happened.
        handle.cmds.blocking_send(PageCmd::Click(toggle)).unwrap();
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected Updated after hide");
        };
        assert!(!html.contains("Additional Link 1"), "{html}");
    }

    #[test]
    fn inline_modules_execute_against_the_dom() {
        let (out, outcome) = page(
            "<body><div id=t></div><script type=module>\
             const t = document.getElementById('t');\
             t.textContent = 'module ran';\
             export const x = 1;\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.modules_skipped, 0);
        assert!(out.contains("module ran"), "{out}");
    }

    #[test]
    fn bare_specifier_imports_reject_honestly() {
        let (out, outcome) = page(
            "<body><p>kept</p><script type=module>\
             import {LitElement} from 'lit';\
             </script></body>",
        );
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        assert!(
            outcome.errors[0].contains("cannot resolve module specifier 'lit'"),
            "{:?}",
            outcome.errors
        );
        assert!(out.contains("kept"), "{out}");
    }

    #[test]
    fn custom_elements_render_through_shadow_dom() {
        // The Lit shape, hand-rolled: define, attachShadow, render into
        // it, slots project, attributes react, clicks work inside the
        // shadow tree.
        let (handle, mut events) = live(
            "<body><greeting-card who=\"sister\"><span slot=name>Ruby</span></greeting-card>\
             <script type=module>\
             class GreetingCard extends HTMLElement {\
               static get observedAttributes() { return ['who']; }\
               constructor() { super(); this.attachShadow({ mode: 'open' }); }\
               connectedCallback() { this.render(); }\
               attributeChangedCallback() { if (this.isConnected) this.render(); }\
               render() {\
                 this.shadowRoot.innerHTML =\
                   '<h2>Hello <slot name=name>stranger</slot>, the ' +\
                   (this.getAttribute('who') || '?') +\
                   '</h2><button id=b>rename</button>';\
                 this.shadowRoot.querySelector('#b').addEventListener('click', () => {\
                   this.setAttribute('who', 'captain');\
                 });\
               }\
             }\
             customElements.define('greeting-card', GreetingCard);\
             </script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected first Updated");
        };
        // Shadow content rendered, slot projected the light child.
        assert!(
            html.contains("Hello <span slot=\"name\">Ruby</span>, the sister"),
            "{html}"
        );
        let button = html
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .unwrap()
            .parse::<usize>()
            .unwrap();

        // A click inside the shadow tree mutates an observed attribute,
        // which re-renders through attributeChangedCallback.
        handle.cmds.blocking_send(PageCmd::Click(button)).unwrap();
        match events.blocking_recv() {
            Some(PageEvt::Updated { html, outcome }) => {
                assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                assert!(html.contains("the captain"), "{html}");
                assert!(html.contains("Ruby"), "{html}");
            }
            other => panic!("expected Updated, got {other:?}"),
        }
    }

    #[test]
    fn webcomponents_loader_feature_probes_pass() {
        // The exact probes @webcomponents/webcomponents-loader.js runs
        // (archive.org ships it). Failing any of them makes a real page
        // try to polyfill itself to death: template content must survive
        // cloneNode, innerHTML, and report as a DocumentFragment; module
        // bundles also want TextEncoder/TextDecoder.
        let (out, outcome) = page(
            r##"<body><div id=out></div><script>
            var t = document.createElement('template');
            var ok = [];
            ok.push('content' in t);
            ok.push(t.content.cloneNode() instanceof DocumentFragment);
            var t2 = document.createElement('template');
            t2.content.appendChild(document.createElement('div'));
            t.content.appendChild(t2);
            var clone = t.cloneNode(true);
            ok.push(clone.content.childNodes.length === 1
                && clone.content.firstChild.content.childNodes.length === 1);
            var holder = document.createElement('div');
            holder.innerHTML = '<template><p>parsed</p></template>';
            ok.push(holder.firstChild.content.childNodes.length === 1);
            var enc = new TextEncoder().encode("héllo ☃");
            ok.push(enc instanceof Uint8Array && enc.length === 10);
            ok.push(new TextDecoder().decode(enc) === "héllo ☃");
            document.getElementById('out').textContent = ok.join(' ');
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("true true true true true true"),
            "feature probes failed: {out}"
        );
    }

    #[test]
    fn intl_shim_formats_honestly_and_passes_feature_detection() {
        // The en-only prelude shim (CLAUDE.md: measured against Boa's
        // half-built intl_bundled and chosen over it). Output is honest
        // English, and the detection surface (resolvedOptions,
        // supportedLocalesOf) exists so i18n libraries take their
        // native path instead of polyfilling or dying.
        let (out, outcome) = page(
            r##"<body><div id=out></div><script>
            var ok = [];
            ok.push(new Intl.NumberFormat().format(1234567.5) === "1,234,567.5");
            ok.push(new Intl.NumberFormat("en", { style: "currency", currency: "USD" }).format(-5) === "-$5.00");
            ok.push(new Intl.NumberFormat("en", { style: "percent" }).format(0.12) === "12%");
            ok.push((1234.5).toLocaleString() === "1,234.5");
            ok.push(new Intl.DateTimeFormat("en", { year: "numeric", month: "short", day: "numeric" })
                .format(new Date(2026, 5, 12)) === "2026-06-12");
            var c = new Intl.Collator("en", { sensitivity: "base", numeric: true });
            ok.push(["b10", "A2", "a1"].sort(c.compare).join(",") === "a1,A2,b10");
            ok.push(new Intl.DisplayNames("en", { type: "region" }).of("US") === "US");
            ok.push(new Intl.PluralRules().select(1) === "one" && new Intl.PluralRules().select(3) === "other");
            ok.push(new Intl.RelativeTimeFormat().format(-2, "day") === "2 days ago");
            ok.push(Intl.NumberFormat.supportedLocalesOf(["en-US", "fr"]).join(",") === "en-US");
            ok.push(new Intl.NumberFormat().resolvedOptions().locale === "en-US");
            document.getElementById('out').textContent = ok.join(' ');
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("true true true true true true true true true true true"),
            "Intl probes failed: {out}"
        );
    }

    #[test]
    fn spa_router_platform_surface_works() {
        // The router-slot recipe (archive.org's router): an <a> as URL
        // parser honoring <base href>, history.state round-tripping
        // with location updates, and `new`-construction of a defined
        // custom element (routers mount pages that way).
        let (out, outcome) = page(
            r##"<html><head><base href="/"></head><body><div id=out></div><script>
            var a = document.createElement('a');
            a.href = '.';
            var parts = [a.pathname, a.protocol];
            history.replaceState({n: 7}, '', '/sub/page?q=1');
            parts.push(history.state.n, location.pathname, location.search);
            class RoutePage extends HTMLElement {}
            customElements.define('route-page', RoutePage);
            var el = new RoutePage();
            parts.push(el.localName, el instanceof RoutePage);
            document.getElementById('out').textContent = parts.join(' ');
            </script></body></html>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("/ https: 7 /sub/page ?q=1 route-page true"),
            "router surface broken: {out}"
        );
    }

    #[test]
    fn a_page_without_scripts_passes_through_untouched() {
        let html = "<body><noscript>no js here</noscript><p>hi</p></body>";
        let (out, outcome) = page(html);
        assert_eq!(out, html);
        assert!(outcome.errors.is_empty());
    }

    #[test]
    fn scripts_build_the_page_with_create_element() {
        let (out, outcome) = page(
            "<body><div id=root></div><script>\
             var d = document.getElementById('root');\
             var h = document.createElement('h1');\
             h.appendChild(document.createTextNode('Built by JS'));\
             d.appendChild(h);\
             </script><noscript>enable javascript!</noscript></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("<h1>Built by JS</h1>"), "{out}");
        // JS ran: noscript content and the script itself are gone.
        assert!(!out.contains("enable javascript"), "{out}");
        assert!(!out.contains("createElement"), "{out}");
    }

    #[test]
    fn inner_html_selectors_and_attributes_work() {
        let (out, outcome) = page(
            "<body><ul id=list class='a b'></ul><script>\
             document.querySelector('ul.a.b').innerHTML = '<li data-n=\"1\">one</li><li>two</li>';\
             var items = document.querySelectorAll('#list > li');\
             document.querySelector('[data-n=\"1\"]').setAttribute('data-seen', 'yes');\
             var p = document.createElement('p');\
             p.textContent = 'count=' + items.length + ' first=' + items[0].textContent;\
             p.classList.add('done');\
             document.body.appendChild(p);\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("count=2 first=one"), "{out}");
        assert!(out.contains("data-seen=\"yes\""), "{out}");
        assert!(out.contains("class=\"done\""), "{out}");
    }

    #[test]
    fn lifecycle_events_and_timers_settle_in_order() {
        let (out, outcome) = page(
            "<body><div id=log></div><script>\
             var log = document.getElementById('log');\
             function add(s) { log.appendChild(document.createTextNode(s + '|')); }\
             document.addEventListener('DOMContentLoaded', function () { add('dcl'); });\
             window.addEventListener('load', function () { add('load'); });\
             setTimeout(function () { add('t50'); }, 50);\
             setTimeout(function () { add('t10'); setTimeout(function () { add('nested'); }, 5); }, 10);\
             Promise.resolve().then(function () { add('micro'); });\
             add('sync');\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // sync first; the microtask drains before DOMContentLoaded; timers
        // run in virtual-time order (10 before 50, nested at 15); load last.
        assert!(out.contains("sync|micro|dcl|t10|nested|t50|load|"), "{out}");
    }

    #[test]
    fn document_write_and_events_bubble() {
        let (out, outcome) = page(
            "<body><div id=outer><button id=b>x</button></div><script>\
             document.write('<p id=wrote>written</p>');\
             var hits = [];\
             document.getElementById('outer').addEventListener('custom', function (e) { hits.push('outer:' + e.target.id); });\
             document.addEventListener('custom', function () { hits.push('doc'); });\
             var ev = new Event('custom', { bubbles: true });\
             document.getElementById('b').dispatchEvent(ev);\
             document.title;\
             var p = document.createElement('p');\
             p.textContent = hits.join(',');\
             document.body.appendChild(p);\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("<p id=\"wrote\">written</p>"), "{out}");
        assert!(out.contains("outer:b,doc"), "{out}");
    }

    #[test]
    fn broken_scripts_do_not_take_the_page_down() {
        let (out, outcome) = page(
            "<body><p>original</p>\
             <script>document.body.appendChild(document.createElement('em')).textContent='one';</script>\
             <script>totally.broken.code();</script>\
             <script>document.body.appendChild(document.createElement('strong')).textContent='two';</script>\
             </body>",
        );
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        assert!(
            outcome.errors[0].starts_with("inline#2"),
            "{:?}",
            outcome.errors
        );
        assert!(out.contains("original"), "{out}");
        assert!(out.contains("<em>one</em>"), "{out}");
        assert!(out.contains("<strong>two</strong>"), "{out}");
    }

    #[test]
    fn external_scripts_are_collected_and_executed() {
        let html = "<head><script src='/app.js'></script>\
             <script type=module src='/mod.js'></script></head>\
             <body><div id=t></div></body>";
        assert_eq!(external_scripts(html), vec!["/app.js".to_string()]);
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![(
            "/app.js".to_string(),
            Some(b"document.getElementById('t').textContent = 'from external';".to_vec()),
        )];
        let (out, outcome) = transform(html, &env);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.modules_skipped, 1);
        assert!(out.contains("from external"), "{out}");
    }

    #[test]
    fn missing_external_is_an_error_not_a_crash() {
        let (out, outcome) = page("<body><p>still here</p><script src='/gone.js'></script></body>");
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0].contains("/gone.js"),
            "{:?}",
            outcome.errors
        );
        assert!(out.contains("still here"), "{out}");
    }

    #[test]
    fn url_storage_and_console_apis_work() {
        let (out, outcome) = page(
            "<body><div id=t></div><script>\
             var u = new URL('../x?q=1#f', location.href);\
             localStorage.setItem('k', 'v');\
             console.log('hello from the page');\
             var sp = new URLSearchParams('a=1&b=two%20words');\
             document.getElementById('t').textContent =\
               u.pathname + ' ' + u.search + ' ' + localStorage.getItem('k') + ' ' + sp.get('b') + ' ' + atob(btoa('roundtrip'));\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("/x ?q=1 v two words roundtrip"), "{out}");
        assert_eq!(outcome.console, vec!["log: hello from the page"]);
    }

    #[test]
    fn without_a_net_grant_fetch_rejects_and_xhr_errors() {
        // PageEnv.net = None: every page request resolves to failure —
        // observably, not silently, and with zero actual I/O.
        let (out, outcome) = page(
            "<body><div id=t></div><script>\
             fetch('/api').catch(function () { document.getElementById('t').textContent = 'rejected'; });\
             var x = new XMLHttpRequest();\
             x.open('GET', '/api');\
             x.onerror = function () { document.getElementById('t').textContent += '+xhr-error'; };\
             x.send();\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("rejected+xhr-error"), "{out}");
        assert_eq!(outcome.fetches, 0);
    }

    #[test]
    fn storage_is_shared_across_pages_per_origin() {
        let storage: WebStorage = Default::default();
        let mut env = PageEnv::bare("https://example.com/one");
        env.storage = Some(storage.clone());
        let (_, outcome) = transform(
            "<body><script>localStorage.setItem('k', 'kept'); sessionStorage.setItem('s', 'too');</script></body>",
            &env,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

        // Same origin, different page: values are there.
        let mut env = PageEnv::bare("https://example.com/two");
        env.storage = Some(storage.clone());
        let (out, _) = transform(
            "<body><div id=t></div><script>document.getElementById('t').textContent = \
             localStorage.getItem('k') + '/' + sessionStorage.getItem('s');</script></body>",
            &env,
        );
        assert!(out.contains("kept/too"), "{out}");

        // Different origin: invisible.
        let mut env = PageEnv::bare("https://other.example.net/");
        env.storage = Some(storage.clone());
        let (out, _) = transform(
            "<body><div id=t></div><script>document.getElementById('t').textContent = \
             String(localStorage.getItem('k'));</script></body>",
            &env,
        );
        assert!(out.contains("null"), "{out}");
    }
}
