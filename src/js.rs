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
pub const WALL_BUDGET: Duration = Duration::from_secs(60);

/// Cumulative *execution* time a page's scripts get before we stop
/// launching more. Measures compute, not wall clock (the wire is async
/// and free), so a slow server can't starve a fast page of its scripts.
pub const COMPUTE_BUDGET: Duration = Duration::from_secs(30);

/// Most loop iterations any single script evaluation may run. Real-world
/// bundle boots measured in the canary use a few hundred thousand at
/// most; ten million leaves headroom while still catching `while(true)`
/// in well under a second.
const LOOP_LIMIT: u64 = 10_000_000;

/// GC policy for the (vendored) boa_gc mark-sweep collector. Boa's stock
/// policy — a 1 MiB floor grown only ~1.43× per collection — thrashes
/// full stop-the-world marks on real bundles: it takes ~19 ever-larger
/// collections to chase a heavy page's growing live set up to steady state
/// (measured on YouTube's 10.5 MiB base: >50% of *execution* time in the
/// GC). The collector is non-generational, so every collection is a full
/// mark over the whole live set — making frequent collection of a large
/// live set the worst case.
///
/// Our policy keeps the floor LOW and the budget tight while the live set is
/// small (so a small live set still gets cheap, cache-friendly frequent
/// collection — a high threshold measurably *regresses* moderate sites like
/// react.dev by ~20%, blowing the cache with accumulated garbage and
/// inflating ALL execution, not just GC), and only lets the budget run loose
/// ([`GC_BIG_GROWTH_PERCENT`]) once the live set exceeds [`GC_BIG_LIVE`] —
/// past cache, where marking is out-of-cache regardless so the only win left
/// is doing fewer (expensive) full marks. `TRUST_GC_FLOOR` (MiB),
/// `TRUST_GC_GROWTH`/`TRUST_GC_BIG_GROWTH` (percent), `TRUST_GC_BIG_LIVE`
/// (MiB) override for tuning. See `js::tests::{engine_profile,gc_floor_win}`.
const GC_FLOOR: usize = 1024 * 1024;
// Below GC_BIG_LIVE the policy matches Boa's stock ~1.43x growth, so moderate
// pages (react.dev's live set is ~4 MiB) behave exactly as before — they have
// nothing to gain from GC tuning anyway (their collections are already cheap).
// The win is reserved for large-live-set heavy pages, which thrash under stock.
const GC_GROWTH_PERCENT: usize = 143;
const GC_BIG_LIVE: usize = 16 * 1024 * 1024;
const GC_BIG_GROWTH_PERCENT: usize = 400;

/// Apply the GC policy to the current thread's collector, honoring the
/// `TRUST_GC_*` overrides.
fn apply_gc_policy() {
    let mib = |name: &str, default: usize| -> usize {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map(|m| m.saturating_mul(1024 * 1024).max(1))
            .unwrap_or(default)
    };
    let pct = |name: &str, default: usize| -> usize {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(default)
    };
    boa_engine::gc::set_gc_threshold(mib("TRUST_GC_FLOOR", GC_FLOOR));
    boa_engine::gc::set_gc_growth_percent(pct("TRUST_GC_GROWTH", GC_GROWTH_PERCENT));
    boa_engine::gc::set_gc_big_growth(
        mib("TRUST_GC_BIG_LIVE", GC_BIG_LIVE),
        pct("TRUST_GC_BIG_GROWTH", GC_BIG_GROWTH_PERCENT),
    );
}

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
#[derive(Debug, Default, Clone)]
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
    // Replace Boa's thrashy GC policy (see GC_FLOOR/GC_GROWTH_PERCENT). The
    // policy is thread-local on the GC, so set it every time we build a page
    // context — the trust-js thread is reused across navigations, and each
    // fresh context's heap starts (near) empty as the old one drops.
    apply_gc_policy();
    (ctx, hooks)
}

/// A runaway loop trips Boa's per-frame loop-iteration cap (`LOOP_LIMIT`).
/// That error is UNCATCHABLE by design (the DoS stop — keeping it catchable
/// would let a page nest fresh-budget calls and spin ~unbounded CPU). The bug
/// these tests pin: when the limit is hit inside ASYNC/CALLBACK context (a
/// promise reaction, the Promise executor, an async fn, an async generator, a
/// native callback, …), Boa's reject paths called `to_opaque` on the
/// uncatchable error, which `panic!`s — and that panic, drained outside any
/// `catch_unwind`, killed the resident page actor (every later click then hit
/// "scripts are no longer running"). The fork now PROPAGATES the uncatchable
/// error out of the job instead; here we assert each shape (a) doesn't panic
/// and (b) leaves the engine HEALTHY (a fresh script still runs) — i.e. the
/// page stays fully live, not a dead snapshot.
#[cfg(test)]
mod runtime_limit_in_async {
    use super::*;

    fn assert_clean(label: &str, src: &str) {
        let (mut ctx, _h) = page_context_with(None);
        ctx.runtime_limits_mut().set_loop_iteration_limit(1000);
        let budget = Budget::new(WALL_BUDGET);
        let mut outcome = Outcome::default();
        run_script(&mut ctx, label, src.as_bytes(), &budget, &mut outcome);
        run_jobs_into(&mut ctx, &budget, &mut outcome);
        assert!(
            !outcome.panicked,
            "{label}: engine panicked instead of propagating cleanly: {:?}",
            outcome.errors
        );
        // The engine must remain usable — the limit aborted one job, not the
        // whole context. A fresh script runs cleanly afterward.
        let mut after = Outcome::default();
        run_script(
            &mut ctx,
            "after",
            b"globalThis.__ok = 2;",
            &budget,
            &mut after,
        );
        assert!(
            !after.panicked && after.errors.is_empty(),
            "{label}: engine unhealthy after the limit: panicked={} {:?}",
            after.panicked,
            after.errors
        );
    }

    const RUNAWAY: &str = "for(var i=0;i<1e9;i++){}";

    #[test]
    fn promise_then() {
        assert_clean(
            "then",
            &format!("Promise.resolve().then(function(){{{RUNAWAY}}});"),
        );
    }
    #[test]
    fn promise_executor() {
        assert_clean(
            "executor",
            &format!("new Promise(function(){{{RUNAWAY}}});"),
        );
    }
    #[test]
    fn promise_finally() {
        assert_clean(
            "finally",
            &format!("Promise.resolve().finally(function(){{{RUNAWAY}}});"),
        );
    }
    #[test]
    fn promise_reject_handler() {
        assert_clean(
            "rejectcb",
            &format!("Promise.reject(0).then(null,function(){{{RUNAWAY}}});"),
        );
    }
    #[test]
    fn promise_all_member() {
        assert_clean(
            "all",
            &format!("Promise.all([Promise.resolve().then(function(){{{RUNAWAY}}})]);"),
        );
    }
    #[test]
    fn async_fn() {
        assert_clean("asyncfn", &format!("(async function(){{{RUNAWAY}}})();"));
    }
    #[test]
    fn async_fn_after_await() {
        assert_clean(
            "asyncawait",
            &format!("(async function(){{await 0;{RUNAWAY}}})();"),
        );
    }
    #[test]
    fn async_generator() {
        assert_clean(
            "asyncgen",
            &format!("(async function*(){{{RUNAWAY}}})().next();"),
        );
    }
    #[test]
    fn sync_generator() {
        assert_clean("gen", &format!("(function*(){{{RUNAWAY}}})().next();"));
    }
    #[test]
    fn native_callback() {
        assert_clean(
            "sort",
            &format!("[3,1,2].sort(function(){{{RUNAWAY}return 0;}});"),
        );
    }
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
// timers) is built *in JavaScript* by PRELUDE on top of them. The win is
// keeping the arena out of Boa's GC (nodes never become GC objects) and
// writing the platform concisely in the language it's specced in — NOT
// engine portability (Boa is the engine; a showstopper means forking it,
// see CLAUDE.md).

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
    /// The shared subresource cache, so the page's own `fetch()` can join
    /// an in-flight/done request for a chunk we already have.
    cache: std::sync::Arc<crate::http::PageCache>,
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
/// 96 covered its full boot (62 observed); 256 covers a large static
/// module graph too (css3test's `tests.js` statically imports 154 spec
/// modules — under 96 the graph never linked). Still a hard envelope
/// against runaway pages; `subresource_allowed` separately blocks
/// private-address pivots regardless of count. PROVISIONAL — her call.
const MAX_PAGE_FETCHES: usize = 256;

/// Most import specifiers we'll speculatively prefetch from one module's
/// source. Bounds the wasted bandwidth of a false-positive scan; the
/// per-page `MAX_PAGE_FETCHES` cap still governs the total.
const MAX_SPECULATIVE_IMPORTS: usize = 64;

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
        ("__dom_computed", 2, sys_computed_style),
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
        ("__css_parse", 1, sys_css_parse),
        ("__css_supports_selector", 1, sys_css_supports_selector),
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

/// `getComputedStyle` backing: the cascade's computed value for one property
/// (inheritance + UA defaults for tracked props; inline-only for the rest),
/// or null when unset. The prelude falls back to inline style on null.
fn sys_computed_style(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_str(args, 1, ctx);
    let dom = page_dom(ctx);
    let d = dom.borrow();
    Ok(
        match arg_node(&d, args, 0).and_then(|id| d.computed_value_resolved(id, &name)) {
            Some(v) => str_value(&v),
            None => JsValue::null(),
        },
    )
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

/// `__css_parse(text)` → the CSSOM rule tree as a JSON string (the prelude
/// `JSON.parse`s it into CSSStyleRule/CSSMediaRule/… for `<style>.sheet`).
/// Uses the same CSS tokenizing as the cascade, so what CSSOM reports and
/// what the cascade honors stay one source of truth.
fn sys_css_parse(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let css = arg_str(args, 0, ctx);
    Ok(str_value(&crate::dom::parse_cssom_json(&css)))
}

/// `__css_supports_selector(selector)` → whether the selector engine can
/// parse it. Backs `CSS.supports("selector(…)")` — honest about the subset
/// we actually evaluate.
fn sys_css_supports_selector(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let sel = arg_str(args, 0, ctx);
    Ok(JsValue::from(crate::dom::selector_parses(&sel)))
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
    phase(&format!("src: PAGE-SYNC {}", request.url));
    handle.block_on(crate::http::fetch(&request)).ok()
}

/// Get a module body for `resolved` THROUGH the shared cache, BLOCKING
/// the page thread. This is the ENTRY-module bootstrap path only: it runs
/// once, before the job loop, where there are no sibling jobs to overlap
/// with — so blocking here costs nothing. The in-graph loader uses
/// `module_fetch` + `.await` instead (concurrent). A hit — the initial
/// prefetch seeded it, or a speculative prefetch is already fetching it —
/// skips the network entirely (and costs no cap). A miss starts a fresh,
/// cap-/`subresource_allowed`-gated request.
fn load_module_body(
    ctx: &mut Context,
    cache: &std::sync::Arc<crate::http::PageCache>,
    resolved: &url::Url,
) -> Option<std::sync::Arc<crate::http::CachedResp>> {
    if let Some(f) = cache.peek(resolved) {
        // Ready (seeded) needs no runtime; in-flight is driven via the
        // handle. No-net pages only ever have ready entries.
        let handle = ctx
            .realm()
            .host_defined()
            .get::<PageNet>()
            .map(|n| n.handle.clone());
        return crate::http::PageCache::block_on_fetch(handle.as_ref(), f);
    }
    let (handle, request) = page_net_prepare(ctx, resolved.as_str(), String::from("GET"), None)?;
    let f = cache.fetch(&handle, request.url);
    crate::http::PageCache::block_on_fetch(Some(&handle), f)
}

/// The shared fetch for a module url, acquired under a brief context
/// borrow so the in-graph loader can `.await` it WITHOUT holding the
/// borrow — letting the spec's concurrently-enqueued sibling imports
/// overlap. A cache hit (seeded / speculatively prefetched / already in
/// flight) joins the existing request; a miss starts a fresh,
/// cap-/`subresource_allowed`-gated one. Returns the `Shared` future to
/// await; `None` only when a brand-new fetch is blocked (cap/private).
fn module_fetch(
    ctx: &mut Context,
    cache: &std::sync::Arc<crate::http::PageCache>,
    resolved: &url::Url,
) -> Option<crate::http::SharedFetch> {
    if let Some(f) = cache.peek(resolved) {
        return Some(f);
    }
    let (handle, request) = page_net_prepare(ctx, resolved.as_str(), String::from("GET"), None)?;
    Some(cache.fetch(&handle, request.url))
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
    // GET dedup: a chunk the page re-`fetch()`es (the bundler warms up its
    // own modulepreloads this way) JOINS the cache's single request for
    // it instead of re-downloading. No new request, no cap spend. Misses
    // — arbitrary API GETs — fall through to a normal, uncached fetch.
    if method == "GET"
        && body.is_none()
        && let Some(shared) = peek_page_cache(ctx, &url_arg)
    {
        phase(&format!("src: PAGE-CACHE {url_arg}"));
        let realm = ctx.realm().clone();
        let job = NativeAsyncJob::with_realm(
            async move |ctx_cell: &RefCell<&mut Context>| {
                let outcome = shared.await;
                let mut guard = ctx_cell.borrow_mut();
                let value = match outcome {
                    Ok(c) => cached_to_array(&c, &mut guard),
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
        return Ok(promise.into());
    }
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
            phase(&format!("src: PAGE-ASYNC {}", request.url));
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

/// `document.cookie` getter: the jar's non-HttpOnly name=value pairs
/// for this exact host (RAM-only jar; see http.rs). Empty without a net grant.
fn sys_cookie_get(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let cookies = ctx
        .realm()
        .host_defined()
        .get::<PageNet>()
        .map(|n| crate::http::cookies_for_js(&n.page))
        .unwrap_or_default();
    Ok(str_value(&cookies))
}

/// `document.cookie = "..."`: store in the RAM-only exact-host jar so
/// later reads and matching requests see it.
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

/// Same `[status, content_type, body_text]` shape from a cached response.
fn cached_to_array(c: &crate::http::CachedResp, ctx: &mut Context) -> JsValue {
    let vals = vec![
        JsValue::from(f64::from(c.status)),
        str_value(&c.content_type),
        str_value(&String::from_utf8_lossy(&c.body)),
    ];
    JsArray::from_iter(vals, ctx).into()
}

/// An existing shared-cache entry (in-flight or done) for a page GET, or
/// None. Lets the page's own `fetch()` JOIN a request for a chunk we
/// already have/are fetching, without creating a cache entry for an
/// uncached API GET (a miss falls through to a normal, uncached fetch).
fn peek_page_cache(ctx: &mut Context, target: &str) -> Option<crate::http::SharedFetch> {
    let host = ctx.realm().host_defined();
    let net = host.get::<PageNet>()?;
    let resolved = net.page.join(target).ok()?;
    net.cache.peek(&resolved)
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

/// Print a JS-phase timeline marker against the shared trace clock, when
/// `TRUST_NET_TRACE=1`. Pairs with the net trace so a single load's wall
/// time can be attributed (compute vs network wait, per phase).
fn phase(label: &str) {
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        eprintln!("js : @{:>6}ms {label}", crate::http::trace_ms());
    }
}

/// Everything a page transformation needs from the outside world.
pub struct PageEnv {
    pub url: String,
    pub viewport: (u16, u16),
    /// Terminal cell size in pixels (width, height) — the picker's font
    /// size. The reported CSS-pixel viewport is `viewport * cell_px`, so
    /// the page sees a window proportional to the real terminal (a wide
    /// terminal looks like a wide browser; SPAs/responsive layouts and
    /// infinite-scrollers measure against it). 8x16 nominal in tests.
    pub cell_px: (u16, u16),
    /// Pre-fetched external scripts keyed by raw `src` attribute
    /// (None = the fetch failed).
    pub externals: Vec<(String, Option<Vec<u8>>)>,
    /// Pre-fetched `<link rel=stylesheet>` bodies keyed by raw `href`,
    /// for the display/visibility cascade (failed fetches are absent —
    /// fail-open, the page just renders un-hidden).
    pub sheets: Vec<(String, String)>,
    /// The shared per-page subresource cache (seeded with the announced
    /// module preloads). The module loader reads module bodies from it,
    /// speculative import prefetch fills it ahead of the loader, and the
    /// page's own `fetch()` joins it — so no chunk is fetched twice and
    /// the module graph downloads concurrently instead of one RTT at a
    /// time. See `http::PageCache`.
    pub cache: std::sync::Arc<crate::http::PageCache>,
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
            cell_px: (8, 16),
            externals: Vec::new(),
            sheets: Vec::new(),
            cache: std::sync::Arc::new(crate::http::PageCache::default()),
            net: None,
            storage: None,
        }
    }
}

/// Format a rejected/thrown JS value for an error message: its display
/// form plus an Error's `.stack` when present (diagnostics are worthless
/// without it). Shared by promise-rejection tracking and module rejection.
fn describe_rejection(v: &JsValue, ctx: &mut Context) -> String {
    let mut s = format!("{}", v.display());
    if let Some(o) = v.as_object()
        && let Ok(st) = o.get(boa_engine::js_string!("stack"), ctx)
        && !st.is_undefined()
        && !st.is_null()
        && let Ok(st) = st.to_string(ctx)
        && !st.is_empty()
    {
        s.push_str(&format!("\n{}", st.to_std_string_lossy()));
    }
    s
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
                        PromiseState::Rejected(v) => describe_rejection(&v, context),
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

/// Best-effort scan of JS module source for its STATIC import specifiers,
/// so we can prefetch them concurrently BEFORE Boa's loader (which we run
/// serially, to keep module loading atomic) asks for them one at a time.
/// This is the engine's "preload scanner": it turns the static module
/// graph from sum-of-RTTs into depth-of-graph-times-RTT, like a browser.
/// Heuristic and fail-open — a false positive prefetches a dud (cached as
/// a failure, harmless), a false negative just falls back to the serial
/// fetch. Recognizes `import…from "x"`, `export…from "x"`, and bare
/// `import "x"`; only path-like specifiers (`/`, `./`, `../`, absolute
/// URL) are kept, which filters most stray matches.
///
/// Dynamic `import(...)` is deliberately NOT scanned: a router entry
/// dynamic-imports EVERY route (archive.org: 51), almost all lazy and
/// never loaded at boot, so prefetching them is pure waste and would
/// starve the real-module cap. The boot-time dynamic imports a page DOES
/// fire stay serial here — parallelizing those is the Boa concurrency
/// follow-up (they need the loader to interleave safely), not the
/// scanner's job.
fn scan_module_imports(src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let n = src.len();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut i = 0;
    while i < n {
        // Anchor on the keywords that introduce a module specifier.
        let kw = if src[i..].starts_with(b"from") {
            4
        } else if src[i..].starts_with(b"import") {
            6
        } else {
            i += 1;
            continue;
        };
        // Word boundary before (so `transform`/`reimport` don't match).
        if i > 0 && is_ident(src[i - 1]) {
            i += 1;
            continue;
        }
        let mut j = i + kw;
        while j < n && (src[j] as char).is_whitespace() {
            j += 1;
        }
        // `import(` is a DYNAMIC import — skip it (see the doc comment).
        if kw == 6 && j < n && src[j] == b'(' {
            i += kw;
            continue;
        }
        // A quoted string literal here is a STATIC specifier.
        if j < n && (src[j] == b'"' || src[j] == b'\'' || src[j] == b'`') {
            let quote = src[j];
            let start = j + 1;
            let mut k = start;
            while k < n && src[k] != quote && src[k] != b'\n' {
                k += 1;
            }
            if k < n && src[k] == quote {
                if let Ok(spec) = std::str::from_utf8(&src[start..k]) {
                    let path_like = spec.starts_with('/')
                        || spec.starts_with("./")
                        || spec.starts_with("../")
                        || spec.starts_with("http");
                    if path_like && !spec.contains("${") {
                        out.push(spec.to_string());
                    }
                }
                i = k + 1;
                continue;
            }
        }
        i += kw; // not a specifier site; step past the keyword
    }
    out
}

/// Fire concurrent prefetches for `body`'s import specifiers into the
/// shared cache, resolved against `base`. Cap- and `subresource_allowed`-
/// gated and counted like any page fetch; deduped against in-flight
/// entries. The loader, reaching these modules moments later, finds them
/// already in flight (or done) instead of paying a fresh round trip.
fn speculate_imports(ctx: &mut Context, base: &url::Url, body: &[u8]) {
    let specs = scan_module_imports(body);
    if specs.is_empty() {
        return;
    }
    let host = ctx.realm().host_defined();
    let Some(net) = host.get::<PageNet>() else {
        return;
    };
    let handle = net.handle.clone();
    let cache = net.cache.clone();
    for spec in specs.into_iter().take(MAX_SPECULATIVE_IMPORTS) {
        let Some(resolved) = base
            .join(&spec)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
        else {
            continue;
        };
        if cache.peek(&resolved).is_some() {
            continue; // already prefetched, in flight, or done
        }
        if !crate::http::subresource_allowed(&net.page, &resolved)
            || net.fetched.get() >= MAX_PAGE_FETCHES
            || net.budget.exhausted()
        {
            continue;
        }
        net.fetched.set(net.fetched.get() + 1);
        phase(&format!("src: SPECULATE {resolved}"));
        cache.prefetch(&handle, resolved);
    }
}

/// ES modules over the web: imports resolve against the importing
/// module's URL (carried as the Source path), fetch through the page's
/// net grant with the same caps and guards as everything else, and
/// cache per page. Bare specifiers ("lit") have no resolution here and
/// reject — honestly.
struct WebModuleLoader {
    page: Option<url::Url>,
    /// Parsed `Module` records, deduped per page (Boa needs one record
    /// per specifier — re-parsing trips an identity assert).
    modules: RefCell<std::collections::HashMap<String, boa_engine::Module>>,
    /// The shared subresource cache: module bodies arrive here from the
    /// initial prefetch, from speculative import prefetch (fired ahead of
    /// this loader as each module is scanned), and from this loader's own
    /// fetches. A `Shared` future per URL means a body in flight is never
    /// re-requested.
    body: std::sync::Arc<crate::http::PageCache>,
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
        if let Some(cached) = self.modules.borrow().get(&key) {
            phase(&format!("module CACHE-HIT {key}"));
            return Ok(cached.clone());
        }

        let fail = || {
            boa_engine::JsNativeError::typ()
                .with_message(format!("module fetch failed or blocked: {key}"))
        };
        // The body comes from the shared cache: already there if the
        // initial prefetch announced it or a speculative prefetch ran
        // ahead of us; otherwise we start it now. Either way one request
        // per URL — the bundler's own `fetch()` of this chunk joins it.
        //
        // We acquire the shared fetch under a brief borrow, then `.await`
        // it WITHOUT holding the context, so the sibling module-load jobs
        // the spec's `inner_load` enqueued concurrently actually overlap
        // on the network instead of each parking the page thread in turn.
        // (The old code BLOCKED here — `block_on_fetch`'s synchronous recv
        // serialized the whole graph into one-fetch-per-RTT and let a
        // single slow chunk stall everything. Awaiting is what the async
        // `ModuleLoader` trait + the concurrent job executor are for.)
        let hit = self.body.peek(&resolved).is_some();
        let fetch = {
            let mut ctx = context.borrow_mut();
            module_fetch(&mut ctx, &self.body, &resolved)
        }
        .ok_or_else(fail)?;
        let cached = fetch.await.ok().ok_or_else(fail)?;
        phase(&format!(
            "module {} {key}",
            if hit { "CACHED" } else { "NETWORK" }
        ));
        // Re-check after the await: a concurrent load of the SAME url may
        // have parsed it while we were on the wire. Module identity per
        // url MUST be stable — Boa's per-referrer `loaded_modules` holds a
        // single record and asserts it — so the first job to reach `parse`
        // wins and every other sharer returns that record. Parse + insert
        // below is synchronous (no `.await` between this check and the
        // insert), so exactly one job parses a given url; the re-check
        // can't race it.
        if let Some(cached) = self.modules.borrow().get(&key) {
            return Ok(cached.clone());
        }
        let module = {
            let mut ctx = context.borrow_mut();
            // Prime the NEXT wave before parsing this one: prefetch this
            // module's own imports so they're in flight before the loader
            // asks for each.
            speculate_imports(&mut ctx, &resolved, &cached.body);
            boa_engine::Module::parse(
                Source::from_bytes(&cached.body).with_path(std::path::Path::new(&key)),
                None,
                &mut ctx,
            )?
        };
        self.modules.borrow_mut().insert(key, module.clone());
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
    register: Option<(&Rc<WebModuleLoader>, &str)>,
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
    // Register the entry module in the loader cache under its own URL BEFORE
    // evaluating it, so a later `import` of the entry's own URL (archive.org
    // does this) reuses THIS instance instead of re-fetching + parsing a
    // second Module record for the same specifier — which both wastes a
    // fetch per chunk (cascading through its import subgraph) and risks the
    // duplicate-record identity assert. One tracked Module, as intended.
    if let Some((loader, key)) = register {
        loader
            .modules
            .borrow_mut()
            .insert(key.to_string(), module.clone());
    }
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
                // Was a generic string while the cyclic-module `Debug`
                // recursion made formatting a module value dangerous; the
                // fork patched that, so report the real reason + stack.
                Some(describe_rejection(&err, ctx))
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
    phase("load_page start (DOM parse)");
    let page_url = env.url.as_str();
    let viewport = env.viewport;
    let cell_px = env.cell_px;
    let externals = &env.externals;
    let mut outcome = Outcome::default();
    let dom = Rc::new(RefCell::new(Dom::parse_document(html)));
    // The CSS-pixel viewport (cols/rows × the terminal's cell size) that the
    // cascade evaluates `@media` against — the same window the page sees as
    // innerWidth/innerHeight below.
    dom.borrow_mut().set_viewport_px(
        u32::from(viewport.0) * u32::from(cell_px.0.max(1)),
        u32::from(viewport.1) * u32::from(cell_px.1.max(1)),
    );
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
    let loader = Rc::new(WebModuleLoader {
        page: parsed_url.clone(),
        modules: RefCell::new(std::collections::HashMap::new()),
        body: env.cache.clone(),
    });
    let (mut ctx, hooks) = page_context_with(Some(loader.clone()));
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
                cache: env.cache.clone(),
            });
        }
    }
    if let Err(err) = register_syscalls(&mut ctx) {
        outcome.errors.push(format!("syscalls: {err}"));
        return Err(outcome);
    }
    // CSS-pixel viewport from the real terminal: cols/rows times the
    // terminal's cell pixel size (the picker's font size; 8x16 nominal).
    let cfg = format!(
        "globalThis.__trust_cfg = {{ url: \"{}\", ua: \"TRust/0.1\", width: {}, height: {} }};",
        esc_js(page_url),
        u32::from(viewport.0) * u32::from(cell_px.0.max(1)),
        u32::from(viewport.1) * u32::from(cell_px.1.max(1)),
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
    phase(&format!(
        "prelude done; {} top-level scripts",
        scripts.len()
    ));

    for (i, (src, inline, type_attr)) in scripts.iter().enumerate() {
        let script_started = Instant::now();
        if std::env::var_os("TRUST_NET_TRACE").is_some() {
            let label = src.as_deref().unwrap_or("<inline>");
            let ty = type_attr.as_deref().unwrap_or("classic");
            phase(&format!("script[{i}] start {ty} {label}"));
        }
        if !is_classic(type_attr) {
            // ES modules execute for real now; non-module foreign types
            // (importmap, json, ...) still skip.
            if type_attr.as_deref().is_some_and(|t| t.trim() == "module") {
                match src {
                    Some(src) => {
                        // Load the entry module THROUGH the loader via a
                        // synthetic importer (register) — do NOT let Boa
                        // run it as its own entry. archive.org dynamically
                        // imports its OWN entry URL; registering it under
                        // its URL keeps ONE tracked Module (a second record
                        // for the same specifier trips an identity assert
                        // whose panic stack-overflows formatting the cyclic
                        // graph). Its body comes from the shared cache (it
                        // was seeded as a classic <script>), and we
                        // speculatively prefetch its imports so the first
                        // wave is already in flight.
                        let resolved = parsed_url
                            .as_ref()
                            .and_then(|b| b.join(src).ok())
                            .filter(|u| matches!(u.scheme(), "http" | "https"));
                        let cached = resolved
                            .as_ref()
                            .and_then(|u| load_module_body(&mut ctx, &env.cache, u));
                        match (resolved, cached) {
                            (Some(resolved), Some(cached)) => {
                                let path = resolved.to_string();
                                speculate_imports(&mut ctx, &resolved, &cached.body);
                                run_module(
                                    &mut ctx,
                                    src,
                                    &cached.body,
                                    &path,
                                    &budget,
                                    &mut outcome,
                                    Some((&loader, &path)),
                                );
                            }
                            _ => outcome.modules_skipped += 1,
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
                            None,
                        );
                    }
                }
                if outcome.panicked {
                    break;
                }
                run_jobs_into(&mut ctx, &budget, &mut outcome);
            }
            phase(&format!(
                "script[{i}] done +{}ms",
                script_started.elapsed().as_millis()
            ));
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
        phase(&format!(
            "script[{i}] done +{}ms",
            script_started.elapsed().as_millis()
        ));
    }

    if !outcome.panicked {
        // Fire DOMContentLoaded, then RETURN at this "interactive"
        // boundary: scripts have run and the page shell is built, but the
        // post-DOMContentLoaded settle (which DRAINS background network —
        // e.g. an SPA's data fetches) is deferred to `settle_page`. The
        // actor paints the shell here before that settle, so a data-driven
        // page shows its chrome immediately instead of waiting for every
        // background fetch (see `page_actor`). The one-shot `transform`
        // path calls `settle_page` straight away, so its render is
        // unchanged.
        phase("scripts done; DOMContentLoaded");
        run_script(
            &mut ctx,
            "DOMContentLoaded",
            b"__trust.readyState = \"interactive\"; __trust.fire(document, \"DOMContentLoaded\", true);",
            &budget,
            &mut outcome,
        );
    }

    drain_js_side(&mut ctx, &mut outcome);
    drain_rejections(&hooks, &mut outcome);
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

/// Drain the post-DOMContentLoaded lifecycle: settle (background network +
/// timers), the `load` event, then a final job drain. This is where a
/// page's in-flight fetches (and the DOM mutations they cause) complete —
/// split out of `load_page` so the actor can paint the interactive shell
/// FIRST and let this run after. Idempotent-ish: a page with no pending
/// network/timers settles instantly here (so the shell == the full
/// render), which keeps the one-shot path and non-network tests unchanged.
fn settle_page(page: &mut LoadedPage) {
    if !page.outcome.panicked {
        phase("settle start");
        settle(&mut page.ctx, &page.budget, MAX_TICKS, &mut page.outcome);
        phase("settle done");
        run_script(
            &mut page.ctx,
            "load",
            b"__trust.readyState = \"complete\"; __trust.fire(window, \"load\", false);",
            &page.budget,
            &mut page.outcome,
        );
        // The load handler can schedule post-load work as a timer — the very
        // common `setTimeout(finish, 0)` pattern (every WPT testharness page
        // completes this way; many SPAs defer init like this). Drain to
        // quiescence (timers + microtasks), not just the microtask queue, so
        // that work runs as part of the load settle. Still budget/MAX_TICKS-
        // bounded, and once quiescent timers stay frozen at rest.
        settle(&mut page.ctx, &page.budget, MAX_TICKS, &mut page.outcome);
        phase("load done");
    }

    drain_js_side(&mut page.ctx, &mut page.outcome);
    drain_rejections(&page.hooks, &mut page.outcome);
    phase(&format!(
        "load finished; last DOM mutation was @{}ms",
        crate::dom::last_mutation_ms()
    ));
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        // Reading the live set after a forced collection tells us the page's
        // true GC footprint — used to size the GC policy (see GC_BIG_LIVE).
        let before = boa_engine::gc::gc_profile();
        boa_engine::gc::force_collect();
        let after = boa_engine::gc::gc_profile();
        phase(&format!(
            "GC: {} collections, {:?} total; live set ~{} MiB",
            before.0,
            before.1,
            after.2 / (1024 * 1024)
        ));
    }
    page.outcome.fetches = page
        .ctx
        .realm()
        .host_defined()
        .get::<PageNet>()
        .map_or(0, |n| n.fetched.get());
}

/// Drain due timers and microtasks until quiet, budget-bounded.
/// Job errors (exceptions escaping microtasks — e.g. an async component
/// update throwing) are REAL page errors: collect, don't discard.
fn settle(ctx: &mut Context, budget: &Budget, max_ticks: usize, outcome: &mut Outcome) {
    let mut ticks = 0;
    loop {
        run_jobs_into(ctx, budget, outcome);
        if budget.exhausted() || ticks >= max_ticks {
            phase(&format!(
                "settle: {ticks} ticks, exhausted={}",
                budget.exhausted()
            ));
            break;
        }
        match ctx.eval(Source::from_bytes(b"__trust.tick(1000)")) {
            Ok(v) if v.to_boolean() => ticks += 1,
            _ => {
                phase(&format!("settle: {ticks} ticks, quiescent"));
                break;
            }
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
    // catch_unwind the drain, like `run_script`/`run_module` already do: a
    // job's execution can panic on a Boa edge (e.g. a host uncatchable limit
    // reaching a reject path the fork doesn't yet propagate cleanly), and that
    // panic must cost the page, NOT unwind and kill the resident actor thread
    // — which would leave the page a dead engine ("scripts no longer running"
    // on every click). The asymmetry of leaving THIS drain unprotected while
    // `eval`/module-eval were wrapped is exactly what made archive.org's
    // promise-reaction limit kill the engine. The DOM mutations so far stay
    // serializable (RefCell guards release during unwind).
    let drained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match (handle, exec) {
            (Some(handle), Some(exec)) => {
                let remaining = budget.remaining();
                if remaining.is_zero() {
                    return Ok(());
                }
                let cell = RefCell::new(ctx);
                handle.block_on(async {
                    tokio::time::timeout(remaining, exec.run_jobs_async(&cell))
                        .await
                        .unwrap_or(Ok(())) // deadline reached: render what we have
                })
            }
            _ => ctx.run_jobs(),
        }
    }));
    match drained {
        Ok(Ok(())) => {}
        Ok(Err(err)) => outcome.errors.push(format!("async job: {err}")),
        Err(_) => {
            outcome.errors.push(String::from(
                "async job: engine panic (Boa bug) — page JS halted",
            ));
            outcome.panicked = true;
        }
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
            settle_page(&mut page);
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
    /// Set a form control's value in the live DOM, then fire input/change.
    SetValue {
        node: usize,
        value: String,
        checked: Option<bool>,
    },
    /// Dispatch a submit event on a live form. If page JS does not
    /// prevent it, the app proceeds with its existing HTTP submit path.
    Submit {
        form: usize,
        submitter: Option<usize>,
    },
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
    /// A dispatch settled without a renderable mutation.
    Settled,
    /// The page did not prevent a form submit; the app should perform
    /// the normal HTTP form submission it already prepared.
    SubmitDefault,
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
    // First paint: if the interactive shell is already a live page (has
    // clickables), emit it NOW — before `settle_page` drains background
    // network. A data-driven SPA (e.g. archive.org, which then serially
    // paginates ~14 collection requests) shows its chrome immediately
    // instead of blocking first paint on every background fetch. For a
    // page with no post-interactive work the shell == the settled render,
    // so this is a harmless duplicate; for a static article (no
    // clickables) we skip it and fall through to the Static path. Timers
    // stay frozen at rest — `settle_page` advances them once, here, not in
    // the idle dispatch loop.
    let (shell, shell_clickable) = extract_live(&mut page);
    // The shell render reports wall-clock elapsed for the status bar, but
    // we must NOT write that back onto `page.outcome.elapsed`: that field
    // is the cumulative-COMPUTE accumulator `run_script`'s budget gate
    // reads, and `settle_page` still has to fire the `load` event through
    // it. Stamping wall time here (≈ shell-paint wall, which dwarfs the 2s
    // COMPUTE_BUDGET) made `load` skip every time — the page settled but
    // its load handlers never ran. Stamp the CLONE we send instead.
    let mut shell_outcome = page.outcome.clone();
    shell_outcome.elapsed = page.started.elapsed();
    let painted_live = shell_clickable
        && evts
            .blocking_send(PageEvt::Updated {
                html: shell,
                outcome: shell_outcome,
            })
            .is_ok();
    if shell_clickable && !painted_live {
        return; // app dropped the handle while we painted the shell
    }
    // We've consumed the current DOM into the shell render; clear the
    // dirty bit so we can tell whether the settle below actually changes
    // anything. A page with no background work settles to the SAME DOM —
    // we then skip the redundant second emit, so non-network pages emit
    // exactly one load render (the shell), preserving the existing actor
    // contract; only a page that mutates during settle (archive's tiles)
    // gets a second, filled-in render.
    let _ = page.dom.borrow_mut().take_dirty();

    // Drain the rest of the lifecycle (background network + timers).
    settle_page(&mut page);
    page.outcome.elapsed = page.started.elapsed();
    let changed = page.dom.borrow_mut().take_dirty();
    if painted_live && !changed {
        // Shell already reflects the settled page; nothing new to send.
    } else {
        let (out, has_clickables) = extract_live(&mut page);
        let outcome = std::mem::take(&mut page.outcome);
        if !has_clickables && !painted_live {
            let _ = evts.blocking_send(PageEvt::Static { html: out, outcome });
            return;
        }
        if evts
            .blocking_send(PageEvt::Updated { html: out, outcome })
            .is_err()
        {
            return;
        }
    }

    // The dispatch loop: blocked (zero CPU) until the app speaks or
    // drops the handle. Timers are frozen at rest by design — they only
    // advance inside a dispatch.
    while let Some(cmd) = cmds.blocking_recv() {
        match cmd {
            PageCmd::Click(node) => {
                let nav = dispatch_click_in(&mut page, node);
                drain_js_side(&mut page.ctx, &mut page.outcome);
                if let Some(url) = take_script_navigation(&mut page).or(nav) {
                    if evts.blocking_send(PageEvt::Navigate(url)).is_err() {
                        return;
                    }
                    continue; // app decides; we stay alive until dropped
                }
                if !finish_dispatch(&mut page, &evts) {
                    return;
                }
            }
            PageCmd::SetValue {
                node,
                value,
                checked,
            } => {
                dispatch_form_set_in(&mut page, node, &value, checked);
                drain_js_side(&mut page.ctx, &mut page.outcome);
                if let Some(url) = take_script_navigation(&mut page) {
                    if evts.blocking_send(PageEvt::Navigate(url)).is_err() {
                        return;
                    }
                    continue;
                }
                if !finish_dispatch(&mut page, &evts) {
                    return;
                }
            }
            PageCmd::Submit { form, submitter } => {
                let prevented = dispatch_submit_in(&mut page, form, submitter);
                drain_js_side(&mut page.ctx, &mut page.outcome);
                if page.outcome.panicked {
                    let _ = evts
                        .blocking_send(PageEvt::Trouble(std::mem::take(&mut page.outcome.errors)));
                    return;
                }
                if let Some(url) = take_script_navigation(&mut page) {
                    if evts.blocking_send(PageEvt::Navigate(url)).is_err() {
                        return;
                    }
                    continue;
                }
                if !prevented {
                    if evts.blocking_send(PageEvt::SubmitDefault).is_err() {
                        return;
                    }
                    continue;
                }
                if !finish_dispatch(&mut page, &evts) {
                    return;
                }
            }
        }
    }
}

fn finish_dispatch(page: &mut LoadedPage, evts: &tokio::sync::mpsc::Sender<PageEvt>) -> bool {
    if page.outcome.panicked {
        // Engine bug: degrade to static, last render stands.
        let _ = evts.blocking_send(PageEvt::Trouble(std::mem::take(&mut page.outcome.errors)));
        return false;
    }
    let dirty = page.dom.borrow_mut().take_dirty();
    if dirty {
        let (out, _) = extract_live(page);
        let outcome = std::mem::take(&mut page.outcome);
        return evts
            .blocking_send(PageEvt::Updated { html: out, outcome })
            .is_ok();
    }
    if !page.outcome.errors.is_empty() {
        return evts
            .blocking_send(PageEvt::Trouble(std::mem::take(&mut page.outcome.errors)))
            .is_ok();
    }
    evts.blocking_send(PageEvt::Settled).is_ok()
}

fn take_script_navigation(page: &mut LoadedPage) -> Option<String> {
    let v = page
        .ctx
        .eval(Source::from_bytes(b"__trust.takeNavigation()"))
        .ok()?;
    if v.is_null_or_undefined() {
        return None;
    }
    let s = v.to_string(&mut page.ctx).ok()?.to_std_string_lossy();
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn prepare_dispatch(page: &mut LoadedPage) {
    page.budget.rearm(DISPATCH_BUDGET);
    if let Some(net) = page.ctx.realm().host_defined().get::<PageNet>() {
        net.fetched.set(0);
    }
    let _ = page.dom.borrow_mut().take_dirty();
}

fn js_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn dispatch_form_set_in(page: &mut LoadedPage, node: usize, value: &str, checked: Option<bool>) {
    prepare_dispatch(page);
    let checked = checked
        .map(|v| if v { "true" } else { "false" })
        .unwrap_or("null");
    let call = format!("__trust.formSet({node}, {}, {checked})", js_string(value));
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        page.ctx.eval(Source::from_bytes(call.as_bytes()))
    })) {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => page.outcome.errors.push(format!("form input: {err}")),
        Err(_) => {
            page.outcome.errors.push(String::from(
                "form input: engine panic (Boa bug) — page JS halted",
            ));
            page.outcome.panicked = true;
            return;
        }
    }
    let mut dispatch_outcome = Outcome::default();
    settle(
        &mut page.ctx,
        &page.budget,
        DISPATCH_TICKS,
        &mut dispatch_outcome,
    );
    page.outcome.errors.extend(dispatch_outcome.errors);
}

fn dispatch_submit_in(page: &mut LoadedPage, form: usize, submitter: Option<usize>) -> bool {
    prepare_dispatch(page);
    let submitter = submitter
        .map(|id| id.to_string())
        .unwrap_or_else(|| String::from("null"));
    let call = format!("__trust.formSubmit({form}, {submitter})");
    let prevented = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        page.ctx.eval(Source::from_bytes(call.as_bytes()))
    })) {
        Ok(Ok(v)) => v.to_boolean(),
        Ok(Err(err)) => {
            page.outcome.errors.push(format!("form submit: {err}"));
            false
        }
        Err(_) => {
            page.outcome.errors.push(String::from(
                "form submit: engine panic (Boa bug) — page JS halted",
            ));
            page.outcome.panicked = true;
            return true;
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
    prevented
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

    let has_forms = everyone.iter().copied().any(|d| {
        matches!(
            dom.tag_name(d),
            Some("form" | "input" | "button" | "select" | "textarea")
        )
    });
    let has_any = !clickable.is_empty() || has_forms;
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
    prepare_dispatch(page);

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
            : t === 8 ? new Comment(id)
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
            this.composed = !!(opts && opts.composed);
            this.defaultPrevented = false;
            this.target = null;
            this.currentTarget = null;
            this.isTrusted = false;
            // CustomEvent.detail (and UIEvent.detail) default to null, not
            // undefined, when not supplied.
            this.detail = opts && "detail" in opts ? opts.detail : null;
            this.timeStamp = 0;
            // Per-interface EventInit members (MouseEventInit.clientX,
            // KeyboardEventInit.key, MessageEventInit.data, …) become event
            // properties. We don't model each interface's dictionary, so copy
            // any extra init members generically — without clobbering the
            // standard fields set above.
            if (opts) for (const k in opts) if (!(k in this)) this[k] = opts[k];
        }
        // Cancelling only takes effect on a cancelable event (spec): a
        // preventDefault on a non-cancelable event is a no-op. Real code and
        // the platform both read defaultPrevented to decide whether to run
        // the default action.
        preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
        // Legacy alias: returnValue is the inverse of defaultPrevented;
        // assigning false cancels (honoring cancelable), true can't un-cancel.
        get returnValue() { return !this.defaultPrevented; }
        set returnValue(v) { if (!v) this.preventDefault(); }
        stopPropagation() { this.__stop = true; }
        stopImmediatePropagation() { this.__stop = this.__stopNow = true; }
        // Legacy DOM init for events made via document.createEvent(): deprecated
        // but still used by feature-detection (webcomponentsjs probes it) and a
        // lot of older code. initCustomEvent is the CustomEvent variant.
        initEvent(type, bubbles, cancelable) {
            this.type = String(type);
            this.bubbles = !!bubbles;
            this.cancelable = !!cancelable;
            this.defaultPrevented = false;
        }
        initCustomEvent(type, bubbles, cancelable, detail) {
            // type is a mandatory WebIDL argument.
            if (arguments.length < 1) throw new TypeError("initCustomEvent requires a type");
            this.type = String(type);
            this.bubbles = !!bubbles;
            this.cancelable = !!cancelable;
            this.detail = detail === undefined ? null : detail;
            this.defaultPrevented = false;
        }
        // The same walk dispatch() bubbles along: shadow hop via __host.
        composedPath() {
            if (!this.target) return [];
            const path = [this.target];
            let p = this.target instanceof Node ? (this.target.parentNode || this.target.__host) : null;
            while (p) { path.push(p); p = p.parentNode || p.__host; }
            if (this.target !== g) path.push(g);
            return path;
        }
        // Legacy positional init for the typed createEvent() interfaces. The
        // type-specific tail (view/detail/coords/keys) is accepted and stored
        // generically; the first three args are the real Event init.
        initUIEvent(type, bubbles, cancelable, view, detail) {
            this.initEvent(type, bubbles, cancelable);
            this.view = view; this.detail = detail;
        }
        initMouseEvent(type, bubbles, cancelable, view, detail, sx, sy, cx, cy, ctrl, alt, shift, meta, button, related) {
            this.initEvent(type, bubbles, cancelable);
            this.view = view; this.detail = detail;
            this.screenX = sx; this.screenY = sy; this.clientX = cx; this.clientY = cy;
            this.ctrlKey = ctrl; this.altKey = alt; this.shiftKey = shift; this.metaKey = meta;
            this.button = button; this.relatedTarget = related;
        }
        initKeyboardEvent(type, bubbles, cancelable, view, key) {
            this.initEvent(type, bubbles, cancelable);
            this.view = view; this.key = key;
        }
    }
    // The standard Event-interface hierarchy. Real browsers expose all of
    // these as constructable globals with distinct prototypes; code (and
    // polyfills like webcomponentsjs) reference `window.MouseEvent`, do
    // `new KeyboardEvent(...)`, and check `e instanceof MouseEvent`. They
    // inherit Event's constructor (which already copies init-dict members),
    // so `new MouseEvent("click", { clientX: 5 })` sets `clientX`.
    class UIEvent extends Event {}
    class MouseEvent extends UIEvent {}
    class PointerEvent extends MouseEvent {}
    class WheelEvent extends MouseEvent {}
    class DragEvent extends MouseEvent {}
    class KeyboardEvent extends UIEvent {}
    class FocusEvent extends UIEvent {}
    class InputEvent extends UIEvent {}
    class TouchEvent extends UIEvent {}
    class CompositionEvent extends UIEvent {}
    class PopStateEvent extends Event {}
    class HashChangeEvent extends Event {}
    class MessageEvent extends Event {}
    class ErrorEvent extends Event {}
    class PromiseRejectionEvent extends Event {}
    class ProgressEvent extends Event {}
    class SubmitEvent extends Event {}
    class StorageEvent extends Event {}
    class AnimationEvent extends Event {}
    class TransitionEvent extends Event {}
    class ClipboardEvent extends Event {}
    class PageTransitionEvent extends Event {}
    class CloseEvent extends Event {}
    // createEvent("MouseEvent") must yield a MouseEvent, etc. (legacy path).
    const EVENT_INTERFACES = {
        Event, CustomEvent: Event, Events: Event, HTMLEvents: Event,
        UIEvent, UIEvents: UIEvent, MouseEvent, MouseEvents: MouseEvent,
        PointerEvent, WheelEvent, DragEvent, KeyboardEvent, KeyEvents: KeyboardEvent,
        FocusEvent, InputEvent, TouchEvent, CompositionEvent, PopStateEvent,
        HashChangeEvent, MessageEvent, ErrorEvent, ProgressEvent, SubmitEvent,
        StorageEvent, AnimationEvent, TransitionEvent, ClipboardEvent,
        PageTransitionEvent, CloseEvent,
    };
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
                catch (e) { trust.errors.push(ev.type + " handler: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
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
    function nearestForm(el) {
        let p = el;
        while (p) {
            if (p.localName === "form") return p;
            p = p.parentNode;
        }
        return null;
    }
    function fireFormEvents(el, withClick) {
        // Toggling a checkbox/radio dispatches a click as part of the user
        // activation, BEFORE input/change. It matters: React detects
        // checkbox/radio changes off the CLICK event (its change plugin's
        // shouldUseClickEvent path), not input/change, so without this a
        // controlled checkbox never fires onChange. The checked value is
        // already set, so listeners read the post-toggle state.
        if (withClick) dispatch(el, new Event("click", { bubbles: true, cancelable: true }), false);
        dispatch(el, new Event("input", { bubbles: true }), false);
        dispatch(el, new Event("change", { bubbles: true }), false);
    }
    // Set a control property as a USER edit would, NOT a script write.
    // Frameworks (React, Vue, Preact) install an instance-level "value
    // tracker" — an own getter/setter that shadows the prototype's — and
    // suppress their onChange when the new value matches what the tracker
    // last saw. A plain `el.value = x` goes THROUGH that tracker, so the
    // change looks like a no-op and onChange never fires. Walking to the
    // prototype accessor and invoking its setter bypasses the instance
    // tracker (the same trick React Testing Library / Enzyme use), so the
    // following input/change event registers as a genuine user change.
    // With no tracker installed this is identical to `el[prop] = value`.
    function nativeSet(el, prop, value) {
        let p = Object.getPrototypeOf(el);
        while (p) {
            const d = Object.getOwnPropertyDescriptor(p, prop);
            if (d) {
                if (typeof d.set === "function") { d.set.call(el, value); return; }
                break;
            }
            p = Object.getPrototypeOf(p);
        }
        el[prop] = value;
    }
    trust.formSet = function (id, value, checked) {
        const el = wrap(id);
        if (!el) return false;
        value = value === null || value === undefined ? "" : String(value);
        const tag = el.localName;
        const type = String(el.type || "").toLowerCase();
        const isToggle = tag === "input" && (type === "checkbox" || type === "radio");
        let changed = false;
        if (isToggle) {
            const want = !!checked;
            if (type === "radio" && want && el.name) {
                const scope = nearestForm(el) || g.document;
                for (const r of scope.querySelectorAll("input")) {
                    if (r !== el && String(r.type || "").toLowerCase() === "radio" && r.name === el.name && r.checked) {
                        nativeSet(r, "checked", false);
                        changed = true;
                    }
                }
            }
            if (el.checked !== want) { nativeSet(el, "checked", want); changed = true; }
        } else if (tag === "select") {
            for (const o of el.querySelectorAll("option")) {
                const ov = o.getAttribute("value") === null ? o.textContent : o.getAttribute("value");
                const want = ov === value;
                if (want !== o.hasAttribute("selected")) {
                    if (want) o.setAttribute("selected", "");
                    else o.removeAttribute("selected");
                    changed = true;
                }
            }
        } else if (tag === "textarea") {
            if (el.textContent !== value) { el.textContent = value; changed = true; }
        } else {
            if (el.value !== value) { nativeSet(el, "value", value); changed = true; }
        }
        if (changed) fireFormEvents(el, isToggle);
        return changed;
    };
    trust.formSubmit = function (formId, submitterId) {
        const form = wrap(formId);
        if (!form) return false;
        const ev = new Event("submit", { bubbles: true, cancelable: true });
        ev.submitter = submitterId === null || submitterId === undefined ? null : wrap(submitterId);
        dispatch(form, ev, false);
        return ev.defaultPrevented;
    };

    // --- the DOM classes over the syscall boundary ---
    // Custom-element upgrades return the element being upgraded from
    // the base constructor (the standard polyfill trick), so
    // `class X extends HTMLElement { constructor(){ super(); ... } }`
    // initializes the EXISTING wrapper.
    const CE = { defs: new Map(), tags: new Map(), waiting: new Map(), upgrading: null };
    // EventTarget is the root of the node + window hierarchy (Node and Window
    // both extend it), so the spec's listener methods live here ONCE and
    // everything inherits them. It must be declared before Node/Window (class
    // bindings aren't hoisted). Polyfills save/augment the "native" OFF this
    // prototype — ShadyDOM does `L(EventTarget.prototype,"addEventListener")`
    // and installs `__shady_*` accessors here — so nodes inherit those too.
    // (`lsFor`/`dispatch` are hoisted function declarations, defined above.)
    class EventTarget {
        addEventListener(type, fn) {
            // Functions AND `{ handleEvent }` objects (Lit's EventParts register
            // themselves as listeners).
            if (typeof fn === "function" || (fn && typeof fn.handleEvent === "function")) {
                const l = lsFor(this, String(type));
                if (!l.includes(fn)) l.push(fn);
            }
        }
        removeEventListener(type, fn) { const l = lsFor(this, String(type)); const i = l.indexOf(fn); if (i >= 0) l.splice(i, 1); }
        dispatchEvent(ev) { return dispatch(this, ev, false); }
    }
    class Node extends EventTarget {
        constructor(id) {
            super();
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
        // addEventListener/removeEventListener/dispatchEvent are inherited from
        // EventTarget.prototype now (Node extends EventTarget, per spec). Keeping
        // them solely there means a polyfill that augments EventTarget.prototype
        // (ShadyDOM's `__shady_*` accessors) is visible on every node too.
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
        // <canvas> 2d context. We paint no raster, but sites use it to
        // normalise CSS colours (Web Animations sets `ctx.fillStyle = colour`
        // and reads it back) and to measure text. A pass-through stub stores/
        // echoes its properties and no-ops drawing — enough that the code
        // doesn't throw, without pretending to paint. Canvas-only; other tags
        // report no context, so `el.getContext && el.getContext('2d')` probes
        // still fail correctly off-canvas.
        getContext(kind) {
            if (this.tagName !== "CANVAS" || String(kind) !== "2d") return null;
            return this.__ctx2d || (this.__ctx2d = {
                canvas: this,
                fillStyle: "#000000", strokeStyle: "#000000",
                font: "10px sans-serif", globalAlpha: 1, lineWidth: 1,
                lineCap: "butt", lineJoin: "miter", textAlign: "start", textBaseline: "alphabetic",
                save() {}, restore() {}, scale() {}, rotate() {}, translate() {},
                transform() {}, setTransform() {}, resetTransform() {},
                beginPath() {}, closePath() {}, moveTo() {}, lineTo() {},
                bezierCurveTo() {}, quadraticCurveTo() {}, arc() {}, arcTo() {},
                rect() {}, ellipse() {}, fill() {}, stroke() {}, clip() {},
                clearRect() {}, fillRect() {}, strokeRect() {},
                fillText() {}, strokeText() {}, drawImage() {},
                measureText(t) { return { width: String(t).length * 6 }; },
                getImageData() { return { data: new Uint8ClampedArray(0), width: 0, height: 0 }; },
                putImageData() {}, createImageData() { return { data: new Uint8ClampedArray(0), width: 0, height: 0 }; },
                createLinearGradient() { return { addColorStop() {} }; },
                createRadialGradient() { return { addColorStop() {} }; },
                createPattern() { return null; },
                setLineDash() {}, getLineDash() { return []; },
            });
        }
        toDataURL() { return this.tagName === "CANVAS" ? "data:," : undefined; }
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
        // NamedNodeMap, array-like enough for Array.from/iteration/indexing
        // (Alpine's DOM morph does `Array.from(el.attributes)` — undefined
        // here threw ToObject and aborted danbooru's whole render). Values
        // re-read live; the list is a snapshot of names per access.
        get attributes() {
            // Plain loop + snapshot values + `this`-based methods: NO
            // closure capturing a block-scoped local invoked from a native
            // callback (Boa trap #6 — `.map`/getters here aborted the page
            // with a define-opcode OOB panic). Values snapshot per access.
            const names = __dom_attr_names(this.__id) || [];
            const list = [];
            for (let i = 0; i < names.length; i++) {
                const n = names[i];
                const v = __dom_get_attr(this.__id, n);
                list.push({
                    name: n, localName: n, nodeName: n, namespaceURI: null,
                    prefix: null, specified: true, ownerElement: this,
                    value: v, nodeValue: v,
                });
            }
            list.item = function (i) { return this[i] || null; };
            list.getNamedItem = function (nm) {
                for (var j = 0; j < this.length; j++) if (this[j].name === String(nm)) return this[j];
                return null;
            };
            return list;
        }
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
        // <input>'s type IDL attribute defaults to "text" when the content
        // attribute is absent (HTML spec: limited to known values, missing →
        // Text). Code keys off this default constantly — React's change-event
        // plugin does `supportedInputTypes[input.type]` and treats a "" type
        // as a non-text input, so controlled inputs never fire onChange. Other
        // elements keep "" when type is absent.
        get type() {
            const t = this.getAttribute("type");
            if (this.localName === "input") return t === null ? "text" : t.toLowerCase();
            return t === null ? "" : t;
        }
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
            if (this.localName === "template") return wrap(__dom_template_content(this.__id));
            return this.__content;
        }
        // Non-template elements have NO `content` property in the DOM, so a
        // framework's `.content=${…}` property binding (lit's PropertyPart does
        // `element[name] = value`) just sets a plain expando — it must NOT throw
        // the way a getter-without-setter does in strict mode. Templates keep
        // their read-only fragment; ignore writes there.
        set content(v) {
            if (this.localName !== "template") this.__content = v;
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
        insertAdjacentText(p, text) {
            this.insertAdjacentElement(String(p).toLowerCase(), document.createTextNode(String(text)));
        }
        get style() { if (!this.__style) this.__style = styleFor(this); return this.__style; }
        // <style>.sheet — the parsed CSSOM view of this element's CSS text.
        // Re-parsed when textContent changes (code sets textContent then
        // reads .sheet.cssRules). Other elements have no sheet.
        get sheet() {
            if (this.tagName !== "STYLE") return null;
            const text = this.textContent || "";
            if (!this.__sheet || this.__sheetText !== text) {
                this.__sheet = makeStyleSheet(text, this);
                this.__sheetText = text;
            }
            return this.__sheet;
        }
        // `el.style = "color:red"` — the [PutForwards=cssText] behaviour: assigning
        // a string to .style sets inline cssText (a getter-only .style throws in
        // strict mode, which silently broke YouTube's renderers in attr callbacks).
        set style(v) {
            if (v === null || v === undefined || String(v).trim() === "") this.removeAttribute("style");
            else this.setAttribute("style", String(v));
        }
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
        get offsetWidth() { return g.innerWidth; }
        get offsetHeight() { return g.innerHeight; }
        get offsetParent() { return g.document.body; }
        get clientWidth() { return g.innerWidth; }
        get clientHeight() { return g.innerHeight; }
        getBoundingClientRect() { return { x: 0, y: 0, top: 0, left: 0, right: g.innerWidth, bottom: g.innerHeight, width: g.innerWidth, height: g.innerHeight }; }
        getClientRects() { return [this.getBoundingClientRect()]; }
    }

    // CharacterData: the shared text-bearing interface for Text and Comment.
    // `data` is [LegacyNullToEmptyString] — null becomes "" (but undefined
    // stringifies to "undefined"); `length` is the data's UTF-16 length.
    class CharacterData extends Node {
        get data() { return __dom_text(this.__id) || ""; }
        set data(v) { __dom_set_text(this.__id, v === null ? "" : String(v)); }
        get nodeValue() { return this.data; }
        set nodeValue(v) { this.data = v; }
        get length() { return this.data.length; }
        // offset/count are WebIDL `unsigned long` — ToUint32 (`>>> 0`) maps a
        // negative like `-2**32 + 2` to 2; `offset > length` is an
        // IndexSizeError; count is clamped to the remaining length.
        substringData(offset, count) {
            if (arguments.length < 2) throw new TypeError("2 arguments required");
            const d = this.data, o = offset >>> 0;
            if (o > d.length) throw new DOMException("offset out of bounds", "IndexSizeError");
            return d.slice(o, o + Math.min(count >>> 0, d.length - o));
        }
        appendData(s) {
            if (arguments.length < 1) throw new TypeError("1 argument required");
            this.data = this.data + String(s);
        }
        insertData(offset, s) { this.replaceData(offset, 0, s); }
        deleteData(offset, count) { this.replaceData(offset, count, ""); }
        replaceData(offset, count, s) {
            if (arguments.length < 3) throw new TypeError("3 arguments required");
            const d = this.data, o = offset >>> 0;
            if (o > d.length) throw new DOMException("offset out of bounds", "IndexSizeError");
            const c = Math.min(count >>> 0, d.length - o);
            this.data = d.slice(0, o) + String(s) + d.slice(o + c);
        }
    }
    class Text extends CharacterData {}

    class Document extends Node {
        get [Symbol.toStringTag]() { return "HTMLDocument"; }
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
                        createRange: () => new Range(),
                        createNodeIterator: (r, w) => new NodeIterator(r, w),
                        createTreeWalker: (r, w) => new TreeWalker(r, w),
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
        createNodeIterator(root, whatToShow) { return new NodeIterator(root, whatToShow); }
        createDocumentFragment() { return wrap(__dom_create_fragment()); }
        createRange() { return new Range(); }
        getElementById(i) { return wrap(__dom_get_by_id(String(i))); }
        getElementsByName(n) { return this.querySelectorAll("[name=" + String(n) + "]"); }
        createEvent(type) { const C = EVENT_INTERFACES[String(type)] || Event; return new C(""); }
        hasFocus() { return true; }
        // A TRust document is always a visible, focused, non-prerendering
        // foreground page. SPAs routinely DEFER heavy rendering until
        // `visibilityState === "visible"` (or skip work while `prerendering`),
        // so leaving these undefined makes such pages wait forever for a state
        // they never see. (YouTube's kevlar gates feed work on visibility.)
        get visibilityState() { return "visible"; }
        get hidden() { return false; }
        get prerendering() { return false; }
        get wasDiscarded() { return false; }
        get visibilityStates() { return ["visible"]; }
        write(s) { const host = this.body || this.documentElement; if (host) host.insertAdjacentHTML("beforeend", String(s)); }
        writeln(s) { this.write(s + "\n"); }
        open() {} close() {}
    }

    class DocumentFragment extends Node {}
    class Comment extends CharacterData {}
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
    // NodeIterator: a flat document-order walk over a subtree (root first,
    // then descendants). DOMPurify drives sanitization with one of these
    // (`ownerDocument.createNodeIterator(body, …)`). Live, not a snapshot —
    // a sanitizer that removes the current node detaches it, so iteration
    // would stop at that subtree; benign for the content we run it on.
    class NodeIterator {
        constructor(root, whatToShow) {
            this.root = root;
            this.referenceNode = root;
            this.pointerBeforeReferenceNode = true;
            this.whatToShow = (whatToShow === undefined ? 0xFFFFFFFF : whatToShow) >>> 0;
        }
        __shows(n) {
            const bit = n.nodeType === 1 ? 1 : n.nodeType === 3 ? 4 : n.nodeType === 8 ? 128 : 0;
            return (this.whatToShow & bit) !== 0;
        }
        // The document-order successor of `n` within `root`.
        __after(n) {
            let next = n.firstChild;
            if (next) return next;
            let cur = n;
            while (cur && cur !== this.root) {
                if (cur.nextSibling) return cur.nextSibling;
                cur = cur.parentNode;
            }
            return null;
        }
        nextNode() {
            let node = this.referenceNode;
            let before = this.pointerBeforeReferenceNode;
            for (;;) {
                if (before) { before = false; }
                else {
                    const nx = this.__after(node);
                    if (!nx) return null;
                    node = nx;
                }
                if (this.__shows(node)) {
                    this.referenceNode = node;
                    this.pointerBeforeReferenceNode = false;
                    return node;
                }
            }
        }
        previousNode() { return null; }
        detach() {}
    }
    // DOMParser: parse a markup string into a detached document. `text/html`
    // parses into a fresh <body> (the common case — DOMPurify, jQuery, and
    // template libraries feed body-level fragments / whole documents alike).
    // The parsed nodes live in the same arena, so their `ownerDocument`
    // (the real document) carries createNodeIterator/importNode — which is
    // exactly what a sanitizer reaches for through them.
    class DOMParser {
        parseFromString(str, _type) {
            const docEl = g.document.createElement("html");
            const head = g.document.createElement("head");
            const body = g.document.createElement("body");
            docEl.appendChild(head);
            docEl.appendChild(body);
            body.innerHTML = String(str === undefined ? "" : str);
            return {
                nodeType: 9,
                documentElement: docEl,
                head: head,
                body: body,
                createElement: (t) => g.document.createElement(t),
                createTextNode: (s) => g.document.createTextNode(s),
                createComment: (s) => g.document.createComment(s),
                createDocumentFragment: () => g.document.createDocumentFragment(),
                createNodeIterator: (r, w) => new NodeIterator(r, w),
                createTreeWalker: (r, w) => new TreeWalker(r, w),
                createRange: () => new Range(),
                importNode: (n, deep) => n.cloneNode(!!deep),
                getElementsByTagName: (t) => {
                    const want = String(t).toLowerCase();
                    const list = Array.from(docEl.getElementsByTagName(t));
                    // getElementsByTagName matches descendants only; the root
                    // <html> answers `'html'` itself (DOMPurify's whole-doc path).
                    if (docEl.localName === want || want === "*") list.unshift(docEl);
                    return list;
                },
                getElementById: (i) => {
                    for (const e of docEl.querySelectorAll("[id]")) {
                        if (e.id === String(i)) return e;
                    }
                    return null;
                },
                querySelector: (s) => docEl.querySelector(s),
                querySelectorAll: (s) => docEl.querySelectorAll(s),
            };
        }
    }
    // `new XMLSerializer().serializeToString(node)` — the inverse of DOMParser.
    // Delegates to our HTML serializer (outerHTML); documents serialize their root.
    class XMLSerializer {
        serializeToString(node) {
            if (!node) return "";
            if (node.outerHTML !== undefined && node.outerHTML !== null) return node.outerHTML;
            if (node.documentElement) return node.documentElement.outerHTML || "";
            if (node.nodeType === 3 || node.nodeType === 8) return String(node.nodeValue || "");
            return node.innerHTML !== undefined ? node.innerHTML : "";
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
    // define()'s catch-up upgrade, but shadow-piercing: an element
    // rendered into a shadow root BEFORE its definition (archive.org's
    // router does this for the late-loaded page component) is invisible
    // to document.querySelectorAll, so without crossing __sr it would
    // never upgrade — constructed never, rendered never, empty forever.
    function ceUpgradeName(node, name, ctor) {
        if (!node || typeof node !== "object") return;
        if (node instanceof Element) {
            if (node.localName === name) upgradeElement(node, ctor);
            if (node.__sr) ceUpgradeName(node.__sr, name, ctor);
        }
        if (node.childNodes) for (const c of node.childNodes) ceUpgradeName(c, name, ctor);
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
            ceUpgradeName(g.document, name, ctor);
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
    // ---- CSSOM: <style>.sheet.cssRules and the CSSRule hierarchy ----
    // __css_parse(text) → a JSON rule tree (dom.rs parse_cssom_json); we
    // wrap it as the standard CSSRule subclasses so stylesheet-introspection
    // and feature-detection code (css3test's Supports.atrule/descriptorvalue,
    // CSS-in-JS libraries) read real rules. Distinct classes so
    // `constructor.name`/`instanceof` answer correctly.
    function parseCss(text) {
        try { return JSON.parse(__css_parse(String(text || ""))); } catch (e) { return []; }
    }
    // A CSSStyleDeclaration over a rule's [name,value] pairs. Read-mostly
    // (rule edits don't flow back into our cascade); covers the surface
    // introspection code reads: length/item/getPropertyValue/cssText and
    // camelCase-or-kebab property access.
    function ruleStyle(pairs) {
        const order = [];
        const map = new Map();
        for (const pv of pairs || []) {
            if (!map.has(pv[0])) order.push(pv[0]);
            map.set(pv[0], pv[1]);
        }
        const base = {
            get length() { return order.length; },
            item(i) { return order[Number(i)] || ""; },
            getPropertyValue(k) { const v = map.get(String(k).toLowerCase()); return v == null ? "" : v; },
            getPropertyPriority() { return ""; },
            setProperty(k, v) { k = String(k).toLowerCase(); if (!map.has(k)) order.push(k); map.set(k, String(v)); },
            removeProperty(k) { k = String(k).toLowerCase(); const v = map.get(k) || ""; if (map.delete(k)) { const i = order.indexOf(k); if (i >= 0) order.splice(i, 1); } return v; },
            get cssText() { return order.map((k) => k + ": " + map.get(k) + ";").join(" "); },
            set cssText(_) {},
        };
        return new Proxy(base, {
            get(t, p) {
                if (p in t) return t[p];
                if (typeof p === "string") {
                    if (/^\d+$/.test(p)) return order[Number(p)] || "";
                    const v = map.get(kebab(p));
                    return v == null ? "" : v;
                }
                return undefined;
            },
            set(t, p, v) {
                if (p in t) { t[p] = v; return true; }
                if (typeof p === "string") base.setProperty(kebab(p), v);
                return true;
            },
            has(t, p) { return (p in t) || (typeof p === "string" && map.has(kebab(p))); },
        });
    }
    function mediaList(q) {
        q = String(q || "");
        const parts = q.split(",").map((s) => s.trim()).filter(Boolean);
        const ml = {
            get mediaText() { return q; },
            set mediaText(v) { q = String(v); },
            get length() { return parts.length; },
            item(i) { return parts[Number(i)] || null; },
            toString() { return q; },
        };
        parts.forEach((p, i) => { ml[i] = p; });
        return ml;
    }
    // Array subclassing is finicky across engines; a plain array-like with
    // copied indices is safe and gives length/[i]/item/iteration + an
    // honest `constructor.name`.
    class CSSRuleList {
        constructor(items) { this.length = items.length; for (let i = 0; i < items.length; i++) this[i] = items[i]; }
        item(i) { return this[Number(i)] ?? null; }
        [Symbol.iterator]() { return Array.prototype[Symbol.iterator].call(this); }
    }
    function ruleList(items) { return new CSSRuleList(items); }

    class CSSRule { get cssText() { return ""; } get parentStyleSheet() { return null; } }
    class CSSStyleRule extends CSSRule {
        constructor(j) { super(); this.selectorText = j.sel || ""; this.style = ruleStyle(j.d); }
        get type() { return 1; }
        get cssText() { return this.selectorText + " { " + this.style.cssText + " }"; }
    }
    class CSSGroupingRule extends CSSRule {
        constructor(j) { super(); this.cssRules = buildRules(j.r); }
        insertRule(_r, i) { return i || 0; }
        deleteRule() {}
    }
    class CSSMediaRule extends CSSGroupingRule {
        constructor(j) { super(j); this.media = mediaList(j.q); this.conditionText = j.q || ""; }
        get type() { return 4; }
    }
    class CSSSupportsRule extends CSSGroupingRule {
        constructor(j) { super(j); this.conditionText = j.q || ""; }
        get type() { return 12; }
    }
    class CSSContainerRule extends CSSGroupingRule {
        constructor(j) { super(j); this.conditionText = j.q || ""; this.containerName = ""; }
    }
    class CSSLayerBlockRule extends CSSGroupingRule {
        constructor(j) { super(j); this.name = j.q || ""; }
    }
    class CSSFontFaceRule extends CSSRule {
        constructor(j) { super(); this.style = ruleStyle(j.d); }
        get type() { return 5; }
    }
    class CSSPageRule extends CSSRule {
        constructor(j) { super(); this.selectorText = j.sel || ""; this.style = ruleStyle(j.d); }
        get type() { return 6; }
    }
    class CSSKeyframeRule extends CSSRule {
        constructor(j) { super(); this.keyText = j.key || ""; this.style = ruleStyle(j.d); }
        get type() { return 8; }
    }
    class CSSKeyframesRule extends CSSRule {
        constructor(j) { super(); this.name = j.name || ""; this.cssRules = buildRules(j.r); }
        get type() { return 7; }
    }
    class CSSImportRule extends CSSRule {
        constructor(j) { super(); this.href = j.q || ""; this.media = mediaList(""); }
        get type() { return 3; }
    }
    class CSSNamespaceRule extends CSSRule { get type() { return 10; } }
    class CSSCounterStyleRule extends CSSRule {
        constructor(j) { super(); this.name = j.name || ""; this.style = ruleStyle(j.d); }
    }
    class CSSPropertyRule extends CSSRule {
        constructor(j) { super(); this.name = j.name || ""; }
    }
    const RULE_CTORS = {
        style: CSSStyleRule, media: CSSMediaRule, supports: CSSSupportsRule,
        container: CSSContainerRule, layer: CSSLayerBlockRule, scope: CSSGroupingRule,
        document: CSSGroupingRule, "font-face": CSSFontFaceRule, page: CSSPageRule,
        keyframes: CSSKeyframesRule, keyframe: CSSKeyframeRule, import: CSSImportRule,
        namespace: CSSNamespaceRule, "counter-style": CSSCounterStyleRule,
        property: CSSPropertyRule, "font-feature-values": CSSRule,
    };
    function buildRules(arr) {
        const out = [];
        for (const j of arr || []) { const C = RULE_CTORS[j.t]; if (C) out.push(new C(j)); }
        return ruleList(out);
    }

    class CSSStyleSheet {
        constructor() { this.__text = ""; this.__rules = null; this.ownerNode = null; this.media = mediaList(""); }
        get cssRules() { return this.__rules || (this.__rules = ruleList([])); }
        get rules() { return this.cssRules; }
        replace(t) { this.replaceSync(t); return Promise.resolve(this); }
        replaceSync(t) { this.__text = String(t); this.__rules = buildRules(parseCss(this.__text)); sheetSync(this); }
        insertRule(r, i) { this.__text += "\n" + String(r); this.__rules = buildRules(parseCss(this.__text)); sheetSync(this); return i || 0; }
        deleteRule() {}
    }
    function makeStyleSheet(text, owner) {
        const s = new CSSStyleSheet();
        s.__text = String(text || "");
        s.__rules = buildRules(parseCss(s.__text));
        s.ownerNode = owner || null;
        return s;
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

    // Standard DOM node interfaces real browsers expose as global
    // constructors. Code and polyfills (webcomponentsjs walks
    // `["Text","Comment","CDATASection","ProcessingInstruction"]` and reads
    // `window[name].prototype`) reference them and check `instanceof`. We
    // model the common node types on `Node`/`Text`/`Element`; expose the rest
    // with a roughly-correct chain so the constructors and prototypes exist.
    // The global is a `Window` in real browsers: code references the bare
    // `Window` interface (a ReferenceError without it — webcomponentsjs does
    // this) and checks `window instanceof Window`. Window IS an EventTarget.
    class Window extends EventTarget {}
    class CDATASection extends Text {}
    class ProcessingInstruction extends CharacterData {}
    class DocumentType extends Node {}
    class Attr extends Node {}
    // querySelectorAll/getElementsBy* return real Arrays (so .map/.forEach/
    // spread all work); these constructors exist for the `'NodeList' in window`
    // / `instanceof` feature checks code performs. NamedNodeMap is the type of
    // Element.attributes.
    class NodeList {}
    class HTMLCollection {}
    class NamedNodeMap {}
    g.NodeList = NodeList; g.HTMLCollection = HTMLCollection; g.NamedNodeMap = NamedNodeMap;
    g.EventTarget = EventTarget; g.Window = Window; g.CharacterData = CharacterData;
    g.CDATASection = CDATASection; g.ProcessingInstruction = ProcessingInstruction;
    g.DocumentType = DocumentType; g.Attr = Attr;
    g.Node = Node; g.Element = Element; g.HTMLElement = Element;
    g.Text = Text; g.Document = Document; g.HTMLDocument = Document;
    g.DocumentFragment = DocumentFragment; g.Comment = Comment;
    g.Event = Event; g.CustomEvent = Event;
    g.UIEvent = UIEvent; g.MouseEvent = MouseEvent; g.PointerEvent = PointerEvent;
    g.WheelEvent = WheelEvent; g.DragEvent = DragEvent; g.KeyboardEvent = KeyboardEvent;
    g.FocusEvent = FocusEvent; g.InputEvent = InputEvent; g.TouchEvent = TouchEvent;
    g.CompositionEvent = CompositionEvent; g.PopStateEvent = PopStateEvent;
    g.HashChangeEvent = HashChangeEvent; g.MessageEvent = MessageEvent;
    g.ErrorEvent = ErrorEvent; g.PromiseRejectionEvent = PromiseRejectionEvent;
    g.ProgressEvent = ProgressEvent; g.SubmitEvent = SubmitEvent;
    g.StorageEvent = StorageEvent; g.AnimationEvent = AnimationEvent;
    g.TransitionEvent = TransitionEvent; g.ClipboardEvent = ClipboardEvent;
    g.PageTransitionEvent = PageTransitionEvent; g.CloseEvent = CloseEvent;
    g.ShadowRoot = ShadowRoot;
    g.TreeWalker = TreeWalker;
    g.NodeIterator = NodeIterator;
    g.DOMParser = DOMParser;
    g.XMLSerializer = XMLSerializer;
    g.NodeFilter = {
        SHOW_ALL: 0xFFFFFFFF, SHOW_ELEMENT: 1, SHOW_TEXT: 4, SHOW_COMMENT: 128,
        FILTER_ACCEPT: 1, FILTER_REJECT: 2, FILTER_SKIP: 3,
    };
    g.CSSStyleSheet = CSSStyleSheet;
    g.CSSRule = CSSRule; g.CSSStyleRule = CSSStyleRule;
    g.CSSGroupingRule = CSSGroupingRule; g.CSSMediaRule = CSSMediaRule;
    g.CSSSupportsRule = CSSSupportsRule; g.CSSContainerRule = CSSContainerRule;
    g.CSSLayerBlockRule = CSSLayerBlockRule; g.CSSFontFaceRule = CSSFontFaceRule;
    g.CSSPageRule = CSSPageRule; g.CSSKeyframeRule = CSSKeyframeRule;
    g.CSSKeyframesRule = CSSKeyframesRule; g.CSSImportRule = CSSImportRule;
    g.CSSNamespaceRule = CSSNamespaceRule; g.CSSCounterStyleRule = CSSCounterStyleRule;
    g.CSSPropertyRule = CSSPropertyRule; g.CSSRuleList = CSSRuleList;
    g.customElements = customElements;
    g.SVGElement = SVGElement;
    g.HTMLInputElement = HTMLInputElement; g.HTMLSelectElement = HTMLSelectElement;
    g.HTMLTextAreaElement = HTMLTextAreaElement; g.HTMLFormElement = HTMLFormElement;
    g.HTMLAnchorElement = HTMLAnchorElement; g.HTMLImageElement = HTMLImageElement;
    g.HTMLScriptElement = HTMLScriptElement; g.HTMLButtonElement = HTMLButtonElement;
    // The rest of the standard HTML element interface zoo. Browsers expose a
    // constructor for every element kind; boot code patches their prototypes
    // and feature-detects them (YouTube's kevlar reads bare `HTMLTemplateElement`,
    // `HTMLDivElement`, … — a ReferenceError on the first missing one). Each is
    // a distinct Element subclass so prototypes and `instanceof` behave; the
    // guard skips the explicit ones defined above. (Our createElement still
    // returns generic Element instances — these exist for the global surface,
    // not per-tag typing.)
    for (const __n of ["Area","Audio","BR","Base","Body","Canvas","Data","DataList",
        "Details","Dialog","Div","DList","Embed","FieldSet","Heading","Head","HR",
        "Html","IFrame","Label","Legend","LI","Link","Map","Media","Menu","Meta",
        "Meter","Mod","Object","OList","OptGroup","Option","Output","Paragraph",
        "Param","Picture","Pre","Progress","Quote","Slot","Source","Span","Style",
        "TableCaption","TableCell","TableCol","Table","TableRow","TableSection",
        "Template","Time","Title","Track","UList","Unknown","Video"]) {
        const __cn = "HTML" + __n + "Element";
        if (!g[__cn]) {
            const __C = class extends Element {};
            try { Object.defineProperty(__C, "name", { value: __cn }); } catch (e) {}
            g[__cn] = __C;
        }
    }
    g.Image = class { constructor() { return g.document.createElement("img"); } };
    // `new Audio(src)` — the legacy HTMLAudioElement constructor (parallel to
    // Image). Returns an <audio> element with no-op media methods: TRust never
    // plays audio (the video→mpv / no-media ethos), but sites construct one for
    // sound-effect preloading and feature detection — a bare `Audio` reference
    // (ReferenceError when absent) silently broke YouTube's whole renderer family.
    g.Audio = class {
        constructor(src) {
            const el = g.document.createElement("audio");
            if (src !== undefined && src !== null) el.setAttribute("src", String(src));
            el.play = () => Promise.resolve();
            el.pause = () => {};
            el.load = () => {};
            el.canPlayType = () => "";
            return el;
        }
    };
    // Blob/File — a standard data container. We don't do real binary I/O, but
    // sites construct Blobs (object URLs, sanitizer/worker plumbing, feature
    // detection) and a bare `Blob` reference (ReferenceError when absent) silently
    // broke YouTube renderers. Tracks size/type and stringifies its text parts.
    g.Blob = class Blob {
        constructor(parts, opts) {
            this.__parts = Array.isArray(parts) ? parts.slice() : (parts ? [parts] : []);
            let size = 0;
            for (const p of this.__parts) {
                if (typeof p === "string") size += p.length;
                else if (p && typeof p.byteLength === "number") size += p.byteLength;
                else if (p && typeof p.size === "number") size += p.size;
                else size += String(p).length;
            }
            this.size = size;
            this.type = (opts && opts.type) ? String(opts.type).toLowerCase() : "";
        }
        slice(_s, _e, type) { return new g.Blob([], { type: type || this.type }); }
        text() { return Promise.resolve(this.__parts.map((p) => (typeof p === "string" ? p : "")).join("")); }
        arrayBuffer() { return Promise.resolve(new ArrayBuffer(0)); }
        stream() { return null; }
    };
    g.File = class File extends g.Blob {
        constructor(parts, name, opts) { super(parts, opts); this.name = String(name); this.lastModified = (opts && opts.lastModified) || Date.now(); }
    };

    g.window = g; g.self = g; g.top = g; g.parent = g;
    g.document = wrap(0);

    // --- environment ---
    const L = __url_parse(cfg.url, null) || [cfg.url, "", "", "", "", "", "", "", ""];
    const locState = {
        href: L[0], protocol: L[1], host: L[2], hostname: L[3], port: L[4],
        pathname: L[5], search: L[6], hash: L[7], origin: L[8],
    };
    const setLocParts = (p) => {
        locState.href = p[0]; locState.protocol = p[1]; locState.host = p[2];
        locState.hostname = p[3]; locState.port = p[4]; locState.pathname = p[5];
        locState.search = p[6]; locState.hash = p[7]; locState.origin = p[8];
    };
    const withoutHash = (u) => {
        const i = String(u).indexOf("#");
        return i < 0 ? String(u) : String(u).slice(0, i);
    };
    const fireHashChange = (oldURL, newURL) => {
        const ev = new Event("hashchange");
        ev.oldURL = oldURL; ev.newURL = newURL;
        dispatch(g, ev, false);
    };
    const navigateLoc = (u, hashOnly) => {
        if (u === undefined || u === null) return;
        const p = __url_parse(String(u), locState.href);
        if (!p) return;
        const old = locState.href;
        setLocParts(p);
        if (withoutHash(old) === withoutHash(p[0])) {
            if (old !== p[0]) fireHashChange(old, p[0]);
        } else if (!hashOnly) {
            trust.navigation = p[0];
        }
    };
    const updateLoc = (u) => {
        if (u === undefined || u === null) return;
        const p = __url_parse(String(u), locState.href);
        if (p) setLocParts(p);
    };
    const loc = {
        get href() { return locState.href; }, set href(v) { navigateLoc(v, false); },
        get protocol() { return locState.protocol; }, set protocol(_v) {},
        get host() { return locState.host; }, set host(_v) {},
        get hostname() { return locState.hostname; }, set hostname(_v) {},
        get port() { return locState.port; }, set port(_v) {},
        get pathname() { return locState.pathname; }, set pathname(v) { navigateLoc(locState.origin + String(v) + locState.search + locState.hash, false); },
        get search() { return locState.search; }, set search(v) { const q = String(v); navigateLoc(locState.origin + locState.pathname + (q && q[0] === "?" ? q : (q ? "?" + q : "")) + locState.hash, false); },
        get hash() { return locState.hash; }, set hash(v) { const h = String(v); navigateLoc(withoutHash(locState.href) + (h && h[0] === "#" ? h : (h ? "#" + h : "")), true); },
        get origin() { return locState.origin; },
        assign(u) { navigateLoc(u, false); },
        replace(u) { navigateLoc(u, false); },
        reload() { trust.navigation = locState.href; },
        toString() { return locState.href; },
    };
    Object.defineProperty(g, "location", {
        configurable: true, enumerable: true,
        get() { return loc; },
        set(v) { navigateLoc(v, false); },
    });
    trust.takeNavigation = function () { const n = trust.navigation || null; trust.navigation = null; return n; };
    // Host objects must NOT look like plain objects. Real browsers tag
    // them, so `Object.prototype.toString.call(window)` is "[object
    // Window]". Without this they read as "[object Object]", and a
    // library that deep-merges/clones (jQuery UI's widget.extend via
    // isPlainObject) follows window.window / document.defaultView in an
    // infinite cycle until the recursion limit trips (broke danbooru).
    try { g[Symbol.toStringTag] = "Window"; } catch (e) { /* frozen global */ }
    // ...and put the global on Window.prototype so `window instanceof Window`
    // holds and `Window.prototype` reads resolve. The own properties set
    // above are unaffected by the reparent; guard in case the global is frozen.
    try { Object.setPrototypeOf(g, Window.prototype); } catch (e) { /* frozen global */ }
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
    g.history = {
        length: 1, state: null, scrollRestoration: "auto",
        pushState(s, _t, u) { this.state = s === undefined ? null : s; this.length += 1; updateLoc(u); },
        replaceState(s, _t, u) { this.state = s === undefined ? null : s; updateLoc(u); },
        back() {}, forward() {}, go() {},
    };
    // getComputedStyle is now cascade-backed (read-only): __dom_computed
    // returns the inherited / UA-defaulted value for tracked properties and
    // the inline value for the rest, falling back to the element's own inline
    // style on a miss. Was inline-only (it just handed back el.style).
    function computedStyleFor(el) {
        const lookup = (k) => {
            k = kebab(String(k));
            let v = null;
            try { v = __dom_computed(el.__id, k); } catch (e) { v = null; }
            if (v !== null && v !== undefined) return v;
            return el.style.getPropertyValue(k) || "";
        };
        return new Proxy({}, {
            get(_, p) {
                if (typeof p !== "string") return undefined;
                if (p === "getPropertyValue") return (k) => lookup(k);
                if (p === "cssText") return el.getAttribute("style") || "";
                return lookup(p);
            },
            set() { return true; }, // computed style is read-only
            has() { return true; },
        });
    }
    g.getComputedStyle = (el) => (el instanceof Element ? computedStyleFor(el) : makeStyle());
    g.matchMedia = (m) => ({ matches: false, media: String(m), addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {} });
    // window.CSS — feature detection (used across the web, not just
    // css3test). `supports("selector(…)")` runs the real selector engine
    // (honest); the property/value form leans on the style declaration's
    // own acceptance (permissive, like the rest of our CSS surface — we
    // recognize broadly, we don't validate values). `escape` is the CSSOM
    // serialization algorithm.
    function cssEscape(value) {
        value = String(value);
        const len = value.length;
        let out = "";
        for (let i = 0; i < len; i++) {
            const c = value.charCodeAt(i);
            if (c === 0) { out += "�"; continue; }
            if ((c >= 0x1 && c <= 0x1f) || c === 0x7f ||
                (i === 0 && c >= 0x30 && c <= 0x39) ||
                (i === 1 && c >= 0x30 && c <= 0x39 && value.charCodeAt(0) === 0x2d)) {
                out += "\\" + c.toString(16) + " "; continue;
            }
            if (i === 0 && len === 1 && c === 0x2d) { out += "\\" + value.charAt(i); continue; }
            if (c >= 0x80 || c === 0x2d || c === 0x5f ||
                (c >= 0x30 && c <= 0x39) || (c >= 0x41 && c <= 0x5a) || (c >= 0x61 && c <= 0x7a)) {
                out += value.charAt(i); continue;
            }
            out += "\\" + value.charAt(i);
        }
        return out;
    }
    const CSS = {
        escape: cssEscape,
        supports(prop, value) {
            if (value === undefined) {
                let cond = String(prop).trim();
                const m = /^selector\(([\s\S]*)\)$/.exec(cond);
                if (m) return __css_supports_selector(m[1].trim());
                if (cond[0] === "(" && cond[cond.length - 1] === ")") cond = cond.slice(1, -1);
                const c = cond.indexOf(":");
                if (c < 0) return false;
                return CSS.supports(cond.slice(0, c).trim(), cond.slice(c + 1).trim());
            }
            try {
                const d = document.createElement("_").style;
                d.setProperty(String(prop), String(value));
                return d.getPropertyValue(String(prop)) !== "";
            } catch (e) { return false; }
        },
    };
    g.CSS = CSS;
    g.alert = () => {}; g.confirm = () => false; g.prompt = () => null;
    g.scroll = g.scrollTo = g.scrollBy = () => {};
    // DOM Range: feature-detected/instanceof'd at boot, and used for
    // measurement + HTML-string parsing (createContextualFragment, jQuery's
    // `$.parseHTML` fallback). We hold endpoints honestly but approximate
    // geometry with the viewport box like the element rect stubs.
    class Range {
        constructor() {
            this.startContainer = g.document; this.endContainer = g.document;
            this.startOffset = 0; this.endOffset = 0; this.collapsed = true;
            this.commonAncestorContainer = g.document;
        }
        __upd() { this.collapsed = this.startContainer === this.endContainer && this.startOffset === this.endOffset; this.commonAncestorContainer = this.startContainer; }
        setStart(node, off) { this.startContainer = node; this.startOffset = off | 0; this.__upd(); }
        setEnd(node, off) { this.endContainer = node; this.endOffset = off | 0; this.__upd(); }
        setStartBefore(node) { if (node && node.parentNode) this.setStart(node.parentNode, 0); }
        setStartAfter(node) { if (node && node.parentNode) this.setStart(node.parentNode, 0); }
        setEndBefore(node) { if (node && node.parentNode) this.setEnd(node.parentNode, 0); }
        setEndAfter(node) { if (node && node.parentNode) this.setEnd(node.parentNode, 0); }
        selectNode(node) { this.startContainer = this.endContainer = this.commonAncestorContainer = node; this.collapsed = false; }
        selectNodeContents(node) { this.selectNode(node); }
        collapse(toStart) {
            if (toStart) { this.endContainer = this.startContainer; this.endOffset = this.startOffset; }
            else { this.startContainer = this.endContainer; this.startOffset = this.endOffset; }
            this.collapsed = true;
        }
        cloneRange() { const r = new Range(); r.startContainer = this.startContainer; r.endContainer = this.endContainer; r.startOffset = this.startOffset; r.endOffset = this.endOffset; r.collapsed = this.collapsed; r.commonAncestorContainer = this.commonAncestorContainer; return r; }
        cloneContents() { return g.document.createDocumentFragment(); }
        extractContents() { return g.document.createDocumentFragment(); }
        deleteContents() {}
        insertNode(node) { const c = this.startContainer; if (c && c.insertBefore) c.insertBefore(node, (c.childNodes && c.childNodes[this.startOffset]) || null); }
        surroundContents(node) { this.insertNode(node); }
        createContextualFragment(html) { const tpl = g.document.createElement("template"); tpl.innerHTML = String(html); return tpl.content; }
        getBoundingClientRect() { return { x: 0, y: 0, top: 0, left: 0, right: g.innerWidth, bottom: g.innerHeight, width: g.innerWidth, height: g.innerHeight }; }
        getClientRects() { return [this.getBoundingClientRect()]; }
        detach() {}
        toString() { return ""; }
    }
    g.Range = Range;
    class Selection {
        constructor() { this.rangeCount = 0; this.isCollapsed = true; this.type = "None"; this.anchorNode = null; this.focusNode = null; }
        toString() { return ""; }
        getRangeAt() { return new Range(); }
        addRange() {} removeAllRanges() {} removeRange() {} empty() {}
        collapse() {} collapseToStart() {} collapseToEnd() {} selectAllChildren() {}
        setBaseAndExtent() {} extend() {} containsNode() { return false; }
    }
    g.Selection = Selection;
    g.getSelection = () => new Selection();
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
    g.ResizeObserver = class {
        constructor(cb) { this.__cb = cb; this.__dead = false; }
        observe(el) {
            g.setTimeout(() => {
                if (this.__dead) return;
                const r = { x: 0, y: 0, top: 0, left: 0, right: g.innerWidth, bottom: g.innerHeight, width: g.innerWidth, height: g.innerHeight };
                const box = [{ inlineSize: g.innerWidth, blockSize: g.innerHeight }];
                try { this.__cb([{ target: el, contentRect: r, borderBoxSize: box, contentBoxSize: box, devicePixelContentBoxSize: box }], this); }
                catch (e) { trust.errors.push("ResizeObserver: " + ((e && e.message) || e)); }
            }, 0);
        }
        unobserve() {} disconnect() { this.__dead = true; }
    };
    // timeRemaining MUST be positive: idle-chunked work loops are written
    // `while (deadline.timeRemaining() > 0 && hasWork()) process()`, so a 0
    // budget means the loop body never runs, no progress is made, and it
    // reschedules forever (YouTube stamps its feed this way). Report the spec's
    // 50ms cap (constant — a well-behaved chunked loop then drains in one slice,
    // which is what we want since we render once, not per frame).
    g.requestIdleCallback = (fn) => g.setTimeout(() => fn({ didTimeout: false, timeRemaining: () => 50 }), 0);
    g.cancelIdleCallback = (id) => g.clearTimeout(id);

    // --- crypto: getRandomValues + randomUUID + subtle.digest ---
    // No CSPRNG here (text browser, no entropy source): random values
    // are Math.random-derived — fine for request ids / cache keys, NOT
    // real cryptography. subtle.digest IS a true SHA so libraries that
    // hash before they fetch work (archive.org's collection search gates
    // its tile fetch on a SHA-1 request-uid — without this the grid
    // stays empty). Only digest is implemented; the rest of SubtleCrypto
    // stays an honest remainder.
    const __cryptoBytes = (d) => {
        if (d instanceof ArrayBuffer) return new Uint8Array(d.slice(0));
        if (ArrayBuffer.isView(d)) return new Uint8Array(d.buffer.slice(d.byteOffset, d.byteOffset + d.byteLength));
        return new Uint8Array(0);
    };
    const __shaPad = (bytes) => {
        const ml = bytes.length * 8;
        const total = (bytes.length + 1 + 8 + 63) & ~63;
        const m = new Uint8Array(total);
        m.set(bytes); m[bytes.length] = 0x80;
        const dv = new DataView(m.buffer);
        dv.setUint32(total - 8, Math.floor(ml / 0x100000000));
        dv.setUint32(total - 4, ml >>> 0);
        return { m, dv, total };
    };
    function __sha1(bytes) {
        const { dv, total } = __shaPad(bytes);
        let h0 = 0x67452301, h1 = 0xEFCDAB89, h2 = 0x98BADCFE, h3 = 0x10325476, h4 = 0xC3D2E1F0;
        const w = new Uint32Array(80);
        for (let off = 0; off < total; off += 64) {
            for (let i = 0; i < 16; i++) w[i] = dv.getUint32(off + i * 4);
            for (let i = 16; i < 80; i++) { const v = w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]; w[i] = (v << 1) | (v >>> 31); }
            let a = h0, b = h1, c = h2, d = h3, e = h4;
            for (let i = 0; i < 80; i++) {
                let f, k;
                if (i < 20) { f = (b & c) | (~b & d); k = 0x5A827999; }
                else if (i < 40) { f = b ^ c ^ d; k = 0x6ED9EBA1; }
                else if (i < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8F1BBCDC; }
                else { f = b ^ c ^ d; k = 0xCA62C1D6; }
                const t = (((a << 5) | (a >>> 27)) + f + e + k + w[i]) >>> 0;
                e = d; d = c; c = (b << 30) | (b >>> 2); b = a; a = t;
            }
            h0 = (h0 + a) >>> 0; h1 = (h1 + b) >>> 0; h2 = (h2 + c) >>> 0; h3 = (h3 + d) >>> 0; h4 = (h4 + e) >>> 0;
        }
        const out = new Uint8Array(20), o = new DataView(out.buffer);
        o.setUint32(0, h0); o.setUint32(4, h1); o.setUint32(8, h2); o.setUint32(12, h3); o.setUint32(16, h4);
        return out;
    }
    const __SHA256_K = [0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2];
    function __sha256(bytes) {
        const { dv, total } = __shaPad(bytes);
        const h = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
        const w = new Uint32Array(64);
        const rotr = (x, n) => (x >>> n) | (x << (32 - n));
        for (let off = 0; off < total; off += 64) {
            for (let i = 0; i < 16; i++) w[i] = dv.getUint32(off + i * 4);
            for (let i = 16; i < 64; i++) {
                const s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >>> 3);
                const s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >>> 10);
                w[i] = (w[i - 16] + s0 + w[i - 7] + s1) >>> 0;
            }
            let a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g2 = h[6], hh = h[7];
            for (let i = 0; i < 64; i++) {
                const S1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25), ch = (e & f) ^ (~e & g2);
                const t1 = (hh + S1 + ch + __SHA256_K[i] + w[i]) >>> 0;
                const S0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22), maj = (a & b) ^ (a & c) ^ (b & c);
                const t2 = (S0 + maj) >>> 0;
                hh = g2; g2 = f; f = e; e = (d + t1) >>> 0; d = c; c = b; b = a; a = (t1 + t2) >>> 0;
            }
            h[0] = (h[0] + a) >>> 0; h[1] = (h[1] + b) >>> 0; h[2] = (h[2] + c) >>> 0; h[3] = (h[3] + d) >>> 0;
            h[4] = (h[4] + e) >>> 0; h[5] = (h[5] + f) >>> 0; h[6] = (h[6] + g2) >>> 0; h[7] = (h[7] + hh) >>> 0;
        }
        const out = new Uint8Array(32), o = new DataView(out.buffer);
        for (let i = 0; i < 8; i++) o.setUint32(i * 4, h[i]);
        return out;
    }
    g.crypto = {
        getRandomValues(a) {
            if (a && a.length !== undefined) for (let i = 0; i < a.length; i++) a[i] = Math.floor(Math.random() * 0x100000000);
            return a;
        },
        randomUUID() {
            return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (ch) => {
                const r = Math.random() * 16 | 0;
                return (ch === "x" ? r : (r & 0x3 | 0x8)).toString(16);
            });
        },
        subtle: {
            digest(algo, data) {
                const name = (typeof algo === "string" ? algo : (algo && algo.name) || "").toUpperCase();
                const bytes = __cryptoBytes(data);
                if (name === "SHA-1") return Promise.resolve(__sha1(bytes).buffer);
                if (name === "SHA-256") return Promise.resolve(__sha256(bytes).buffer);
                return Promise.reject(new Error("Unsupported digest algorithm: " + name));
            },
        },
    };

    // DOMException — a real constructor (extends Error). core-js's
    // DOMException polyfill does `getBuiltIn("DOMException").prototype`
    // during feature detection; with it undefined that throws ToObject
    // ("cannot convert undefined to object") UNCAUGHT, which tore down
    // danbooru's whole init and stripped its server-rendered post grid.
    const __DE_CODES = {
        IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
        InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
        NotSupportedError: 9, InUseAttributeError: 10, InvalidStateError: 11,
        SyntaxError: 12, InvalidModificationError: 13, NamespaceError: 14,
        InvalidAccessError: 15, SecurityError: 18, NetworkError: 19, AbortError: 20,
        URLMismatchError: 21, QuotaExceededError: 22, TimeoutError: 23,
        InvalidNodeTypeError: 24, DataCloneError: 25,
    };
    class DOMException extends Error {
        constructor(message, name) {
            super(message === undefined ? "" : String(message));
            this.name = name === undefined ? "Error" : String(name);
            this.message = message === undefined ? "" : String(message);
            this.code = __DE_CODES[this.name] || 0;
        }
        get [Symbol.toStringTag]() { return "DOMException"; }
    }
    {
        const legacy = {
            INDEX_SIZE_ERR: 1, DOMSTRING_SIZE_ERR: 2, HIERARCHY_REQUEST_ERR: 3,
            WRONG_DOCUMENT_ERR: 4, INVALID_CHARACTER_ERR: 5, NO_DATA_ALLOWED_ERR: 6,
            NO_MODIFICATION_ALLOWED_ERR: 7, NOT_FOUND_ERR: 8, NOT_SUPPORTED_ERR: 9,
            INUSE_ATTRIBUTE_ERR: 10, INVALID_STATE_ERR: 11, SYNTAX_ERR: 12,
            INVALID_MODIFICATION_ERR: 13, NAMESPACE_ERR: 14, INVALID_ACCESS_ERR: 15,
            VALIDATION_ERR: 16, TYPE_MISMATCH_ERR: 17, SECURITY_ERR: 18, NETWORK_ERR: 19,
            ABORT_ERR: 20, URL_MISMATCH_ERR: 21, QUOTA_EXCEEDED_ERR: 22, TIMEOUT_ERR: 23,
            INVALID_NODE_TYPE_ERR: 24, DATA_CLONE_ERR: 25,
        };
        for (const k in legacy) { DOMException[k] = legacy[k]; DOMException.prototype[k] = legacy[k]; }
    }
    g.DOMException = DOMException;

    g.addEventListener = (t, f) => {
        if (typeof f === "function" || (f && typeof f.handleEvent === "function")) {
            const l = lsFor(g, String(t));
            if (!l.includes(f)) l.push(f);
        }
    };
    g.removeEventListener = (t, f) => { const l = lsFor(g, String(t)); const i = l.indexOf(f); if (i >= 0) l.splice(i, 1); };
    g.dispatchEvent = (ev) => dispatch(g, ev, false);
    // `on<event>` IDL attributes (window.onload = fn). Standard semantics:
    // the attribute is backed by an event listener, so the existing
    // dispatch loop fires it — get returns the handler, set swaps the
    // backing listener. Defining them as properties of the global object
    // is ALSO what lets a module's bare `onload = fn` resolve (Boa's
    // module scope assigns through the global object; without the property
    // it throws "cannot assign to uninitialized global property"). css3test
    // runs its entire suite from `onload`.
    function installEventHandlers(obj, add, remove, types) {
        for (const type of types) {
            let current = null;
            Object.defineProperty(obj, "on" + type, {
                configurable: true,
                enumerable: true,
                get() { return current; },
                set(v) {
                    if (current) remove(type, current);
                    current = typeof v === "function" ? v : null;
                    if (current) add(type, current);
                },
            });
        }
    }
    installEventHandlers(g, g.addEventListener, g.removeEventListener, [
        "load", "unload", "beforeunload", "pageshow", "pagehide",
        "resize", "scroll", "hashchange", "popstate", "message",
        "error", "online", "offline", "focus", "blur", "languagechange",
    ]);
    // GlobalEventHandlers on* IDL attributes on Document and Element (they
    // share Node.prototype). The spec backs each by add/removeEventListener;
    // `this`-relative so a setter registers on the node itself. Two reasons
    // this matters broadly: (1) feature detection — libraries probe event
    // support via `('on'+name) in document` / `in element` (React's change
    // plugin gates the whole `input`-event path on `'oninput' in document`,
    // and without it falls back to a legacy keyup/selectionchange polyfill
    // that never sees our input dispatch → controlled inputs go dead); (2)
    // `el.onclick = fn` assignment works as a real listener.
    function installHandlerProps(proto, types) {
        for (const type of types) {
            Object.defineProperty(proto, "on" + type, {
                configurable: true,
                enumerable: false,
                get() { return (this.__on && this.__on[type]) || null; },
                set(v) {
                    if (!this.__on) this.__on = {};
                    const prev = this.__on[type];
                    if (prev) this.removeEventListener(type, prev);
                    const fn = typeof v === "function" ? v : null;
                    this.__on[type] = fn;
                    if (fn) this.addEventListener(type, fn);
                },
            });
        }
    }
    installHandlerProps(Node.prototype, [
        "click", "dblclick", "auxclick", "contextmenu",
        "mousedown", "mouseup", "mousemove", "mouseover", "mouseout",
        "mouseenter", "mouseleave", "wheel",
        "keydown", "keyup", "keypress",
        "input", "beforeinput", "change", "submit", "reset", "invalid",
        "focus", "blur", "focusin", "focusout",
        "select", "selectionchange",
        "scroll", "load", "error", "abort", "loadstart", "loadend", "progress",
        "drag", "dragstart", "dragend", "dragenter", "dragleave", "dragover", "drop",
        "pointerdown", "pointerup", "pointermove", "pointerover", "pointerout",
        "pointerenter", "pointerleave", "pointercancel", "gotpointercapture", "lostpointercapture",
        "touchstart", "touchend", "touchmove", "touchcancel",
        "animationstart", "animationend", "animationiteration",
        "transitionstart", "transitionend", "transitioncancel",
        "copy", "cut", "paste", "compositionstart", "compositionupdate", "compositionend",
        "play", "pause", "ended", "canplay", "canplaythrough", "durationchange",
        "timeupdate", "volumechange", "waiting", "seeked", "seeking",
        "toggle", "cancel", "close",
    ]);
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
        try { best.fn(); } catch (e) { trust.errors.push("timer: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
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
    // AbortSignal is a real EventTarget (it dispatches "abort"), and the
    // statics `abort`/`timeout`/`any` are widely referenced — YouTube's
    // kevlar bundle reads the bare `AbortSignal` global, a ReferenceError
    // without it.
    class AbortSignal extends EventTarget {
        constructor() { super(); this.aborted = false; this.reason = undefined; this.onabort = null; }
        throwIfAborted() { if (this.aborted) throw this.reason; }
        __abort(reason) {
            if (this.aborted) return;
            this.aborted = true;
            this.reason = reason !== undefined ? reason : new DOMException("signal is aborted without reason", "AbortError");
            const ev = new Event("abort");
            if (typeof this.onabort === "function") { try { this.onabort.call(this, ev); } catch (e) {} }
            this.dispatchEvent(ev);
        }
        static abort(reason) { const s = new AbortSignal(); s.__abort(reason); return s; }
        static timeout(ms) {
            const s = new AbortSignal();
            g.setTimeout(() => s.__abort(new DOMException("signal timed out", "TimeoutError")), Number(ms) || 0);
            return s;
        }
        static any(signals) {
            const s = new AbortSignal();
            for (const sig of signals || []) {
                if (sig && sig.aborted) { s.__abort(sig.reason); break; }
                if (sig && sig.addEventListener) sig.addEventListener("abort", () => s.__abort(sig.reason));
            }
            return s;
        }
    }
    g.AbortSignal = AbortSignal;
    g.AbortController = class AbortController {
        constructor() { this.signal = new AbortSignal(); }
        abort(reason) { this.signal.__abort(reason); }
    };

    // MessageChannel/MessagePort: schedulers (Polymer, React, async libs) use a
    // channel's port to post a macrotask to themselves. We deliver across the
    // pair on a virtual-time timer (a macrotask), which is exactly that role.
    class MessagePort extends EventTarget {
        constructor() { super(); this.onmessage = null; this.__other = null; }
        postMessage(data) {
            const other = this.__other;
            if (!other) return;
            g.setTimeout(() => {
                const ev = new Event("message"); ev.data = data;
                if (typeof other.onmessage === "function") { try { other.onmessage.call(other, ev); } catch (e) {} }
                other.dispatchEvent(ev);
            }, 0);
        }
        start() {} close() { this.__other = null; }
    }
    class MessageChannel {
        constructor() {
            this.port1 = new MessagePort(); this.port2 = new MessagePort();
            this.port1.__other = this.port2; this.port2.__other = this.port1;
        }
    }
    g.MessagePort = MessagePort; g.MessageChannel = MessageChannel;

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

    /// Measure PRELUDE's per-page cost in isolation, and the parse/exec
    /// split (to judge whether a compile cache would even help — note Boa's
    /// `Script` binds to the realm it parsed in, so cross-page reuse isn't
    /// available anyway). Run:
    ///   cargo test --release prelude_cost -- --ignored --nocapture
    #[test]
    #[ignore = "manual measurement"]
    fn prelude_cost() {
        // A faithful page context: DOM arena + syscalls + config script,
        // exactly as `load_page` builds it (minus net/module loader, which
        // PRELUDE doesn't touch at load time).
        fn build_ctx() -> Context {
            let html = r#"<html><head></head><body><p>hi</p><script>1</script></body></html>"#;
            let dom = Rc::new(RefCell::new(Dom::parse_document(html)));
            let mut ctx = page_context_with(None).0;
            {
                let mut host = ctx.realm().host_defined_mut();
                host.insert(PageDom(dom.clone()));
                host.insert(PageStore {
                    map: Default::default(),
                    origin: String::from("https://example.com"),
                });
            }
            register_syscalls(&mut ctx).unwrap();
            let cfg = r#"globalThis.__trust_cfg = { url: "https://example.com/", ua: "TRust/0.1", width: 640, height: 384 };"#;
            ctx.eval(Source::from_bytes(cfg.as_bytes())).unwrap();
            ctx
        }

        const N: u32 = 50;

        // (a) context build + syscalls + config, no PRELUDE.
        let t = Instant::now();
        for _ in 0..N {
            let _c = build_ctx();
        }
        let ctx_build = t.elapsed() / N;

        // (b) PRELUDE total (parse + compile + run) in a fresh context each.
        let t = Instant::now();
        for _ in 0..N {
            let mut c = build_ctx();
            c.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();
        }
        let prelude_total = (t.elapsed() / N).saturating_sub(ctx_build);

        // (c) split parse+compile vs evaluate, fresh context each.
        let (mut parse_acc, mut eval_acc) = (Duration::ZERO, Duration::ZERO);
        for _ in 0..N {
            let mut c = build_ctx();
            let t = Instant::now();
            let script =
                boa_engine::Script::parse(Source::from_bytes(PRELUDE.as_bytes()), None, &mut c)
                    .unwrap();
            parse_acc += t.elapsed();
            let t = Instant::now();
            script.evaluate(&mut c).unwrap();
            eval_acc += t.elapsed();
        }

        // (d) a trivial post-PRELUDE page call, for contrast.
        let mut c = build_ctx();
        c.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();
        let t = Instant::now();
        for _ in 0..N {
            c.eval(Source::from_bytes(
                b"document.querySelector('p').textContent",
            ))
            .unwrap();
        }
        let tiny_call = t.elapsed() / N;

        eprintln!("--- PRELUDE cost (avg of {N}, release recommended) ---");
        eprintln!("PRELUDE size:                      {} bytes", PRELUDE.len());
        eprintln!("context build (+syscalls+config):  {ctx_build:?}");
        eprintln!("PRELUDE total (parse+compile+run): {prelude_total:?}");
        eprintln!("  - parse+compile:                 {:?}", parse_acc / N);
        eprintln!("  - evaluate:                      {:?}", eval_acc / N);
        eprintln!("tiny post-prelude page call:       {tiny_call:?}");
    }

    /// Engine profiler: load an arbitrary JS bundle into a faithful page
    /// context (DOM + syscalls + config + PRELUDE, exactly as `load_page`
    /// builds it) and split its cost into parse / compile / execute, plus
    /// GC stats (vendored boa_gc instrumentation). The bundle runs as a
    /// classic top-level script. Heavy real bundles (the YouTube kevlar
    /// base) won't fully boot without their page environment, but they
    /// still exercise parse+compile of the whole file and a large slab of
    /// execution — enough to find where the engine spends its seconds.
    ///   TRUST_JS_BENCH=/tmp/kevlar.js cargo test --release engine_profile \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore = "manual measurement, needs TRUST_JS_BENCH=<file>"]
    fn engine_profile() {
        let Ok(path) = std::env::var("TRUST_JS_BENCH") else {
            eprintln!("set TRUST_JS_BENCH to a .js file");
            return;
        };
        let src = std::fs::read(&path).unwrap();
        eprintln!("--- engine profile: {path} ({} bytes) ---", src.len());

        // Faithful page context: DOM arena + syscalls + config + PRELUDE.
        let html = r#"<html><head></head><body><div id="content"></div></body></html>"#;
        let dom = Rc::new(RefCell::new(Dom::parse_document(html)));
        let mut ctx = page_context_with(None).0;
        {
            let mut host = ctx.realm().host_defined_mut();
            host.insert(PageDom(dom.clone()));
            host.insert(PageStore {
                map: Default::default(),
                origin: String::from("https://example.com"),
            });
        }
        register_syscalls(&mut ctx).unwrap();
        let cfg = r#"globalThis.__trust_cfg = { url: "https://www.youtube.com/", ua: "TRust/0.1", width: 1600, height: 800 };"#;
        ctx.eval(Source::from_bytes(cfg.as_bytes())).unwrap();
        ctx.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();

        // GC policy comes from page_context_with's apply_gc_policy, honoring
        // TRUST_GC_FLOOR (MiB) / TRUST_GC_GROWTH (percent).
        if std::env::var_os("TRUST_NO_OPT").is_some() {
            ctx.set_optimizer_options(boa_engine::optimizer::OptimizerOptions::empty());
            eprintln!("AST optimizer DISABLED");
        }

        // Per-phase GC deltas: (collections, gc time).
        let g = |a: (usize, Duration, usize), b: (usize, Duration, usize)| (b.0 - a.0, b.1 - a.1);

        let gc0 = boa_engine::gc::gc_profile();
        let t = Instant::now();
        let script = boa_engine::Script::parse(Source::from_bytes(&src), None, &mut ctx).unwrap();
        let parse = t.elapsed();
        let gc1 = boa_engine::gc::gc_profile();

        let t = Instant::now();
        script.codeblock(&mut ctx).unwrap();
        let compile = t.elapsed();
        let gc2 = boa_engine::gc::gc_profile();

        let t = Instant::now();
        let res =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| script.evaluate(&mut ctx)));
        let execute = t.elapsed();
        let gc3 = boa_engine::gc::gc_profile();

        let (pc, pt) = g(gc0, gc1);
        let (cc, ct) = g(gc1, gc2);
        let (ec, et) = g(gc2, gc3);
        eprintln!("parse:           {parse:?}  (gc: {pc} colls, {pt:?})");
        eprintln!("compile:         {compile:?}  (gc: {cc} colls, {ct:?})");
        eprintln!("execute:         {execute:?}  (gc: {ec} colls, {et:?})");
        eprintln!("TOTAL:           {:?}", parse + compile + execute);
        match res {
            Ok(Ok(_)) => eprintln!("result:          ran clean"),
            Ok(Err(e)) => {
                let s = e.to_string();
                eprintln!("result:          threw: {}", &s[..s.len().min(160)]);
            }
            Err(_) => eprintln!("result:          PANIC"),
        }
        eprintln!("--- GC totals ---");
        eprintln!("collections:     {}", gc3.0 - gc0.0);
        eprintln!("gc time:         {:?}", gc3.1 - gc0.1);
        eprintln!("bytes live:      {} MiB", gc3.2 / (1024 * 1024));
    }

    /// Quantify the GC policy win, stock vs TRust default, on two
    /// fully-executing churn workloads (the kevlar profile throws early, so
    /// it under-represents the execute phase where real SPAs live):
    ///   - SMALL live set (~1% retained): the moderate-page case. The TRust
    ///     policy must NOT regress this (it stays stock below GC_BIG_LIVE).
    ///   - LARGE live set (live > GC_BIG_LIVE): the heavy-page case (full
    ///     YouTube keeps a big live set across a long execute). This is where
    ///     stock thrashes full marks and the TRust policy wins.
    ///
    /// `cargo test --release gc_floor_win -- --ignored --nocapture`
    #[test]
    #[ignore = "manual measurement"]
    fn gc_floor_win() {
        // ~1% retained — most framework allocations die young; the live set
        // stays small (a few hundred KiB).
        const SMALL_LIVE: &str = r#"
            var keep = [];
            for (var i = 0; i < 400000; i++) {
                var o = { id: i, name: "item-" + i, tags: [i, i * 2, i * 3],
                          meta: { a: i & 1, b: "x" + (i % 7) } };
                if ((i % 256) === 0) keep.push(o);
            }
            keep.length;
        "#;
        // Build a big live set (>16 MiB) first, then churn lots of garbage on
        // top of it — the heavy-SPA pattern (big retained framework/DOM state
        // + constant short-lived allocation).
        const LARGE_LIVE: &str = r#"
            var keep = [];
            for (var i = 0; i < 120000; i++) {
                keep.push({ id: i, name: "node-" + i, kids: [i, i + 1, i + 2] });
            }
            for (var j = 0; j < 600000; j++) {
                var t = { x: j, y: "g" + j, z: [j] };
            }
            keep.length;
        "#;
        let run = |src: &str, floor: usize, growth: usize, big_live: usize, big: usize| {
            let mut ctx = page_context_with(None).0;
            boa_engine::gc::set_gc_threshold(floor * 1024 * 1024);
            boa_engine::gc::set_gc_growth_percent(growth);
            boa_engine::gc::set_gc_big_growth(big_live * 1024 * 1024, big);
            boa_engine::gc::force_collect(); // reclaim the prior run's heap first
            let g0 = boa_engine::gc::gc_profile();
            let t = Instant::now();
            ctx.eval(Source::from_bytes(src.as_bytes())).unwrap();
            let dt = t.elapsed();
            let g1 = boa_engine::gc::gc_profile();
            (dt, g1.0 - g0.0, g1.1 - g0.1)
        };
        for (label, src) in [
            ("SMALL live set", SMALL_LIVE),
            ("LARGE live set", LARGE_LIVE),
        ] {
            eprintln!("--- {label} ---");
            // Stock Boa: 1 MiB floor, grow-only ~1.43x (big_growth == base).
            let (dt, c, gct) = run(src, 1, 143, 16, 143);
            eprintln!("  stock        : {dt:>12?}  ({c} colls, {gct:?} GC)");
            // TRust default.
            let (dt, c, gct) = run(
                src,
                GC_FLOOR / (1024 * 1024),
                GC_GROWTH_PERCENT,
                GC_BIG_LIVE / (1024 * 1024),
                GC_BIG_GROWTH_PERCENT,
            );
            eprintln!("  trust default: {dt:>12?}  ({c} colls, {gct:?} GC)");
        }
    }

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
                // React ships as two bundles (react + react-dom) that must
                // load together; the dedicated `react_canary` drives them.
                // Loading react-dom alone here would falsely error.
                .filter(|p| {
                    !p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("react"))
                })
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

    /// React canary: boots the real React 18 + ReactDOM UMD bundles and
    /// renders a tree via `createRoot`. React is the biggest framework gap
    /// (no React in the jQuery/D3/Vue/Lit set); its scheduler
    /// (MessageChannel-driven) and synthetic-event system exercise platform
    /// primitives the others don't. Set TRUST_REACT_DEV=1 to load the
    /// development bundles (better error messages, far slower).
    ///
    /// ```sh
    /// cd target/canary
    /// curl -LO https://unpkg.com/react@18/umd/react.production.min.js
    /// curl -LO https://unpkg.com/react-dom@18/umd/react-dom.production.min.js
    /// cargo test --release react_canary -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "manual canary: needs React bundles in target/canary/ and --release timings"]
    fn react_canary() {
        let dev = std::env::var("TRUST_REACT_DEV").is_ok();
        let (rf, rdf) = if dev {
            ("react.development.js", "react-dom.development.js")
        } else {
            ("react.production.min.js", "react-dom.production.min.js")
        };
        let (Ok(react), Ok(react_dom)) = (
            std::fs::read(format!("target/canary/{rf}")),
            std::fs::read(format!("target/canary/{rdf}")),
        ) else {
            eprintln!("no React bundles in target/canary/ — see the doc comment");
            return;
        };
        const PROBE: &str = r##"
            function out(s) { document.body.appendChild(document.createTextNode("\n" + s)); }
            try {
                out("React " + React.version);
                var e = React.createElement;
                var App = function () {
                    var st = React.useState("initial");
                    var count = st[0];
                    var setCount = st[1];
                    React.useEffect(function () {
                        setCount("after-effect");
                    }, []);
                    return e("div", { className: "react-app" }, [
                        e("h1", { key: "h" }, "React renders inside TRust"),
                        e("p", { key: "p", className: "made-by-react" }, "a paragraph from React"),
                        e("span", { key: "s", className: "effect-state" }, "state: " + count),
                        e("button", { key: "b", onClick: function () { setCount("clicked"); } }, "click me"),
                    ]);
                };
                var root = ReactDOM.createRoot(document.getElementById("target"));
                root.render(e(App));
                out("BOOTED-REACT");
            } catch (err) { out("react boot failed: " + (err && (err.stack || err.message) || err)); }
        "##;
        let html = format!(
            "<html><head>\
             <script src=\"/react.js\"></script>\
             <script src=\"/react-dom.js\"></script>\
             </head><body><div id=\"target\">static placeholder</div>\
             <script>{PROBE}</script></body></html>"
        );
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![
            ("/react.js".to_string(), Some(react)),
            ("/react-dom.js".to_string(), Some(react_dom)),
        ];
        let started = Instant::now();
        let (out, outcome) = transform(&html, &env);
        eprintln!(
            "react canary: {:?} total ({:?} in scripts), panicked={}, errors={:?}",
            started.elapsed(),
            outcome.elapsed,
            outcome.panicked,
            outcome.errors,
        );
        for line in out
            .lines()
            .filter(|l| l.contains("BOOTED-REACT") || l.contains("failed") || l.contains("React "))
        {
            eprintln!("  probe> {}", line.trim());
        }
        for marker in ["react-app", "made-by-react", "React renders inside TRust"] {
            if out.contains(marker) {
                eprintln!("  proof in rendered HTML: {marker}");
            }
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
    #[ignore = "manual canary: needs target/canary/vue.global.prod.js"]
    fn vue_template_compiler_canary() {
        let Ok(vue) = std::fs::read("target/canary/vue.global.prod.js") else {
            eprintln!("no target/canary/vue.global.prod.js");
            return;
        };
        let html = "<html><head><script src='/vue.js'></script></head>\n             <body><div id='target'>{{ who }}</div><script>\n             Vue.createApp({ data: function () { return { who: 'TRust template' }; } }).mount('#target');\n             </script></body></html>";
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![("/vue.js".to_string(), Some(vue))];
        let (out, outcome) = transform(html, &env);
        eprintln!("vue template outcome: {:?}", outcome);
        for line in out
            .lines()
            .filter(|line| line.contains("TRust") || line.contains("failed"))
        {
            eprintln!("{}", line.trim());
        }
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            !outcome.panicked,
            "Vue template compiler panicked the engine"
        );
        assert!(out.contains("TRust template"), "{out}");
    }

    #[test]
    #[ignore = "manual canary: needs target/canary/vue.global.prod.js"]
    fn vue_v_for_template_compiler_canary() {
        // The real Vue in-browser compiler path for `v-for`: it builds a
        // `new Function` render whose `with(_ctx)` body feeds `_renderList` a
        // per-item closure. This is what the boa_ast scope-index fix unblocked;
        // before it, the per-item binding resolved to the wrong environment and
        // the list never rendered. Uses the shipped Vue bundle end-to-end.
        let Ok(vue) = std::fs::read("target/canary/vue.global.prod.js") else {
            eprintln!("no target/canary/vue.global.prod.js");
            return;
        };
        let html = "<html><head><script src='/vue.js'></script></head>\n             <body><ul id='target'><li v-for='item in items'>{{ item.name }}</li></ul><script>\n             Vue.createApp({ data: function () { return { items: [{ name: 'alpha' }, { name: 'beta' }, { name: 'gamma' }] }; } }).mount('#target');\n             </script></body></html>";
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![("/vue.js".to_string(), Some(vue))];
        let (out, outcome) = transform(html, &env);
        eprintln!("vue v-for outcome: {outcome:?}");
        for line in out.lines().filter(|line| {
            line.contains("alpha") || line.contains("beta") || line.contains("gamma")
        }) {
            eprintln!("{}", line.trim());
        }
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(!outcome.panicked, "Vue v-for compiler panicked the engine");
        assert!(
            out.contains("alpha") && out.contains("beta") && out.contains("gamma"),
            "{out}"
        );
    }

    #[test]
    fn vue_v_for_style_render_closure_runs() {
        // Vue's full template compiler emits, for `v-for`, a `new Function`-built
        // render whose `with(_ctx)` body declares block-scoped consts captured by
        // a per-item closure that takes the item as a parameter. That shape
        // tripped a Boa scope-index divergence: dynamic functions skipped the
        // `optimize_scope_indicies` pass, so the captured const resolved one
        // environment too high and the per-item closure read the wrong env.
        // Fixed in the vendored boa_ast (`optimize_function_scope_indicies`).
        // This pins the whole transform pipeline, no Vue bundle required.
        let html = r##"<body><ul id="t"></ul><script>
            var _src = {
              renderList: function (arr, cb) { return arr.map(cb).join(','); },
              display: function (x) { return String(x); }
            };
            var makeRender = new Function(
              'lib',
              "const _lib = lib\n" +
              "return function render(ctx) {\n" +
              "  with (ctx) {\n" +
              "    const { renderList: _rl, display: _ds } = _lib;\n" +
              "    return _rl(items, function (it) { return _ds(it.name); });\n" +
              "  }\n" +
              "}"
            );
            var render = makeRender(_src);
            document.getElementById('t').textContent =
              render({ items: [{ name: 'a' }, { name: 'b' }] });
            </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "engine panicked: {outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("a,b"), "{out}");
    }

    #[test]
    fn dom_parser_drives_a_sanitizer_style_walk() {
        // DOMPurify's core path: `new DOMParser().parseFromString(s,'text/html')`,
        // then walk it via `node.ownerDocument.createNodeIterator(body, …)`,
        // then read `body.innerHTML`. Without DOMParser the bundle threw
        // "DOMParser is not defined" / "not a callable function" and dropped
        // its content (archive.org collection pages).
        let html = r##"<body><div id="out"></div><script>
            const doc = new DOMParser().parseFromString('<p>hi</p><b>bold</b><!--c-->', 'text/html');
            const body = doc.body;
            const it = body.ownerDocument.createNodeIterator(
                body, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT | NodeFilter.SHOW_COMMENT, null);
            const seen = [];
            let n;
            while ((n = it.nextNode())) {
                seen.push(n.nodeType === 1 ? n.tagName.toLowerCase()
                        : n.nodeType === 3 ? 't:' + n.textContent : 'comment');
            }
            document.getElementById('out').textContent = seen.join('|') + ' :: ' + body.innerHTML;
        </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // Root (body) first, then descendants in document order.
        assert!(
            out.contains("body|p|t:hi|b|t:bold|comment"),
            "node iterator walked the parsed tree: {out}"
        );
        assert!(
            out.contains("&lt;p&gt;hi&lt;/p&gt;&lt;b&gt;bold&lt;/b&gt;"),
            "{out}"
        );
    }

    // ---- CSSOM surface: CSS.supports, <style>.sheet, on* handlers ----

    #[test]
    fn css_supports_and_escape() {
        let html = r##"<body><pre id="o"></pre><script>
            function L(k,v){ document.getElementById('o').textContent += k+'='+v+'\n'; }
            L('disp', CSS.supports('display','flex'));
            L('emptyval', CSS.supports('display',''));
            L('cond', CSS.supports('(display: grid)'));
            L('sel', CSS.supports('selector(a > b.c)'));
            L('emptysel', CSS.supports('selector()'));
            L('escape', CSS.escape('#id.cls'));
            L('escdigit', CSS.escape('1a'));
        </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("disp=true"), "{out}");
        // An empty value is rejected (the declaration doesn't stick).
        assert!(out.contains("emptyval=false"), "{out}");
        assert!(out.contains("cond=true"), "{out}");
        // The selector form runs the real selector engine.
        assert!(out.contains("sel=true"), "{out}");
        assert!(out.contains("emptysel=false"), "{out}");
        // CSSOM escape: non-ident chars escaped, leading digit hex-escaped.
        assert!(out.contains(r"escape=\#id\.cls"), "{out}");
        assert!(out.contains(r"escdigit=\31 a"), "{out}");
    }

    #[test]
    fn style_sheet_exposes_cssom_rules() {
        // <style>.sheet.cssRules: a style rule (with its declaration block),
        // a nested @media, an @font-face descriptor block, and an UNKNOWN
        // at-rule that must be DROPPED (real browsers omit unrecognized
        // at-rules — feature detection relies on it).
        let html = r##"<body><pre id="o"></pre><style id="s"></style><script>
            function L(k,v){ document.getElementById('o').textContent += k+'='+v+'\n'; }
            var st = document.getElementById('s');
            st.textContent = 'a.x{color:red;margin:0}'
                + '@media (min-width:1px){p{display:block}}'
                + '@font-face{font-family:Z;src:url(z)}'
                + '@totallyunknown foo{bar:1}';
            var r = st.sheet.cssRules;
            L('len', r.length);
            L('r0', r[0].constructor.name);
            L('sel', r[0].selectorText);
            L('r0len', r[0].style.length);
            L('color', r[0].style.getPropertyValue('color'));
            L('media', r[1].constructor.name);
            L('mq', r[1].media.mediaText);
            L('child', r[1].cssRules[0].selectorText);
            L('ff', r[2].constructor.name);
            L('ffdesc', r[2].style.length >= 1);
        </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("len=3"), "unknown at-rule dropped: {out}");
        assert!(out.contains("r0=CSSStyleRule"), "{out}");
        assert!(out.contains("sel=a.x"), "{out}");
        assert!(out.contains("r0len=2"), "{out}");
        assert!(out.contains("color=red"), "{out}");
        assert!(out.contains("media=CSSMediaRule"), "{out}");
        assert!(out.contains("mq=(min-width:1px)"), "{out}");
        assert!(out.contains("child=p"), "{out}");
        assert!(out.contains("ff=CSSFontFaceRule"), "{out}");
        assert!(out.contains("ffdesc=true"), "{out}");
    }

    #[test]
    fn cssom_interface_globals_present() {
        let html = r##"<body><pre id="o"></pre><script>
            var names = ['CSS','CSSStyleSheet','CSSRule','CSSStyleRule','CSSMediaRule',
                'CSSSupportsRule','CSSFontFaceRule','CSSKeyframesRule','CSSKeyframeRule',
                'CSSPageRule','CSSImportRule','CSSRuleList'];
            document.getElementById('o').textContent =
                names.filter(function(n){ return n in window; }).join(',');
        </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        for n in [
            "CSS",
            "CSSStyleSheet",
            "CSSRule",
            "CSSStyleRule",
            "CSSMediaRule",
            "CSSFontFaceRule",
            "CSSKeyframesRule",
            "CSSRuleList",
        ] {
            assert!(out.contains(n), "missing global {n}: {out}");
        }
    }

    #[test]
    fn window_onload_fires_and_var_resolves_in_computed() {
        // The on* IDL attribute fires on load, and getComputedStyle resolves
        // var() (css3test's whole run lives in onload; Supports.variable reads
        // a var()-backed margin-right back).
        let html = r##"<body><pre id="o"></pre>
            <p id="d" style="--x:10px;margin-right:var(--x)"></p><script>
            onload = function(){
                var d = document.getElementById('d');
                document.getElementById('o').textContent =
                    'fired mr=' + getComputedStyle(d).marginRight;
            };
        </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("fired mr=10px"), "{out}");
    }

    #[test]
    fn module_can_assign_window_onload() {
        // A module is strict-mode: a bare `onload = fn` must resolve to the
        // settable global property, not throw "cannot assign to uninitialized
        // global property" (the css3test blocker).
        let html = r##"<body><pre id="o"></pre><script type="module">
            onload = () => { document.getElementById('o').textContent = 'module-onload'; };
        </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("module-onload"), "{out}");
    }

    #[test]
    fn event_preventdefault_honors_cancelable_and_returnvalue() {
        // preventDefault() is a no-op on a non-cancelable event; returnValue
        // mirrors defaultPrevented and cancels (cancelable-gated) when false.
        let html = r##"<body><pre id="o"></pre><script>
            function L(k,v){ document.getElementById('o').textContent += k+'='+v+'\n'; }
            var c = new Event('x', { cancelable: true });
            c.preventDefault(); L('cancelable', c.defaultPrevented);
            var n = new Event('y', { cancelable: false });
            n.preventDefault(); L('noncancelable', n.defaultPrevented);
            var r = new Event('z', { cancelable: true });
            r.returnValue = false; L('returnvalue', r.defaultPrevented);
            L('rv_get', new Event('w').returnValue);
        </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("cancelable=true"), "{out}");
        assert!(
            out.contains("noncancelable=false"),
            "non-cancelable no-op: {out}"
        );
        assert!(out.contains("returnvalue=true"), "{out}");
        assert!(
            out.contains("rv_get=true"),
            "returnValue defaults true: {out}"
        );
    }

    #[test]
    fn custom_event_init_defaults_and_insert_adjacent_text() {
        // initCustomEvent: detail defaults to null, type is mandatory.
        // insertAdjacentText inserts a real text node at each position.
        let html = r##"<body><div id="host">[<span id="m">x</span>]</div>
            <pre id="o"></pre><script>
            function L(k,v){ document.getElementById('o').textContent += k+'='+v+'\n'; }
            var e = document.createEvent ? document.createEvent('CustomEvent') : new CustomEvent('t');
            e.initCustomEvent('t', false, false);
            L('detail', e.detail);
            var threw = false; try { e.initCustomEvent(); } catch (x) { threw = true; }
            L('mandatory', threw);
            var m = document.getElementById('m');
            m.insertAdjacentText('beforebegin', 'B');
            m.insertAdjacentText('afterend', 'A');
            L('host', document.getElementById('host').textContent);
        </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("detail=null"), "detail defaults null: {out}");
        assert!(out.contains("mandatory=true"), "type is mandatory: {out}");
        assert!(out.contains("host=[BxA]"), "text inserted around #m: {out}");
    }

    #[test]
    fn character_data_comment_and_text_have_data_length_and_methods() {
        // Comment/Text are CharacterData: data (null→"" but undefined→
        // "undefined"), UTF-16 length, and the edit methods.
        let html = r##"<body><pre id="o"></pre><script>
            function L(k,v){ document.getElementById('o').textContent += k+'='+v+'\n'; }
            var c = document.createComment('test');
            L('cdata', c.data); L('clen', c.length);
            c.data = null; L('cnull', '['+c.data+']');
            c.data = 'undef-' ; c.appendData('end'); L('append', c.data);
            var t = document.createTextNode('hello');
            t.deleteData(0, 1); L('del', t.data);
            t.insertData(0, 'J'); L('ins', t.data);
            L('sub', document.createTextNode('abcdef').substringData(2, 3));
            L('star', document.createComment('🌠x').length);
        </script></body>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("cdata=test"), "comment data: {out}");
        assert!(out.contains("clen=4"), "comment length: {out}");
        assert!(out.contains("cnull=[]"), "data=null → empty: {out}");
        assert!(out.contains("append=undef-end"), "appendData: {out}");
        assert!(out.contains("del=ello"), "deleteData: {out}");
        assert!(out.contains("ins=Jello"), "insertData: {out}");
        assert!(out.contains("sub=cde"), "substringData: {out}");
        assert!(out.contains("star=3"), "🌠 is 2 UTF-16 units + x: {out}");
    }

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

    /// React synthetic-event system, end-to-end through the page actor:
    /// a `useState` counter whose `onClick` is delegated at the root
    /// container. A real click must bubble to React's root listener, map
    /// the target DOM node back to its fiber, fire the handler, and
    /// re-render. Manual (needs the bundles), like `react_canary`.
    #[test]
    #[ignore = "manual canary: needs React bundles in target/canary/"]
    fn react_clicks_drive_a_state_counter() {
        let (Ok(react), Ok(react_dom)) = (
            std::fs::read("target/canary/react.production.min.js"),
            std::fs::read("target/canary/react-dom.production.min.js"),
        ) else {
            eprintln!("no React bundles in target/canary/");
            return;
        };
        const PROBE: &str = r##"
            var e = React.createElement;
            function App() {
                var st = React.useState(0);
                var n = st[0], setN = st[1];
                return e("div", { className: "react-app" }, [
                    e("h1", { key: "h" }, "count: " + n),
                    e("button", { key: "b", onClick: function () { setN(n + 1); } }, "increment"),
                ]);
            }
            ReactDOM.createRoot(document.getElementById("target")).render(e(App));
        "##;
        let html = format!(
            "<html><head>\
             <script src=\"/react.js\"></script>\
             <script src=\"/react-dom.js\"></script>\
             </head><body><div id=\"target\"></div>\
             <script>{PROBE}</script></body></html>"
        );
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![
            ("/react.js".to_string(), Some(react)),
            ("/react-dom.js".to_string(), Some(react_dom)),
        ];
        let (handle, mut events) = spawn_page(html, env);

        // Drain to the render that actually shows the React tree (the actor
        // may emit a clickable shell first, then the filled render).
        let mut html = String::new();
        for _ in 0..4 {
            match events.blocking_recv() {
                Some(PageEvt::Updated { html: h, outcome })
                | Some(PageEvt::Static { html: h, outcome }) => {
                    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                    html = h;
                    if html.contains("count: 0") {
                        break;
                    }
                }
                other => panic!("expected Updated/Static, got {other:?}"),
            }
        }
        assert!(html.contains("count: 0"), "initial render missing: {html}");
        let button = html
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .expect("a clickable marker for the React button")
            .parse::<usize>()
            .unwrap();

        handle.cmds.blocking_send(PageCmd::Click(button)).unwrap();
        let Some(PageEvt::Updated { html, outcome }) = events.blocking_recv() else {
            panic!("expected Updated after React click");
        };
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            html.contains("count: 1"),
            "React onClick did not re-render through the synthetic event system: {html}"
        );
    }

    /// React controlled inputs: a text field whose `value` is React state
    /// and whose `onChange` writes it back. React installs a value-tracker
    /// on the input (an instance `value` setter) and only fires onChange if
    /// the tracker sees a real change. Our `formSet` sets `el.value` then
    /// fires `input` — this proves the change still reaches React.
    #[test]
    #[ignore = "manual canary: needs React bundles in target/canary/"]
    fn react_controlled_input_reflects_set_value() {
        let (Ok(react), Ok(react_dom)) = (
            std::fs::read("target/canary/react.production.min.js"),
            std::fs::read("target/canary/react-dom.production.min.js"),
        ) else {
            eprintln!("no React bundles in target/canary/");
            return;
        };
        const PROBE: &str = r##"
            var e = React.createElement;
            function App() {
                var st = React.useState("");
                var text = st[0], setText = st[1];
                return e("div", { className: "react-app" }, [
                    e("input", { key: "i", id: "field", value: text,
                        onChange: function (ev) { setText(ev.target.value); } }),
                    e("p", { key: "p", className: "echo" }, "echo: " + text),
                ]);
            }
            ReactDOM.createRoot(document.getElementById("target")).render(e(App));
        "##;
        let html = format!(
            "<html><head>\
             <script src=\"/react.js\"></script>\
             <script src=\"/react-dom.js\"></script>\
             </head><body><div id=\"target\"></div>\
             <script>{PROBE}</script></body></html>"
        );
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![
            ("/react.js".to_string(), Some(react)),
            ("/react-dom.js".to_string(), Some(react_dom)),
        ];
        let (handle, mut events) = spawn_page(html, env);

        let mut html = String::new();
        for _ in 0..4 {
            match events.blocking_recv() {
                Some(PageEvt::Updated { html: h, outcome })
                | Some(PageEvt::Static { html: h, outcome }) => {
                    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                    html = h;
                    if html.contains("echo:") {
                        break;
                    }
                }
                other => panic!("expected Updated/Static, got {other:?}"),
            }
        }
        // Find the input field's actor node id (data-trust-node on controls).
        let field = html
            .split("data-trust-node=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .expect("a data-trust-node on the input")
            .parse::<usize>()
            .unwrap();

        handle
            .cmds
            .blocking_send(PageCmd::SetValue {
                node: field,
                value: "hello".into(),
                checked: None,
            })
            .unwrap();
        let Some(PageEvt::Updated { html, outcome }) = events.blocking_recv() else {
            panic!("expected Updated after SetValue");
        };
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            html.contains("echo: hello"),
            "React onChange did not see the controlled-input value: {html}"
        );
    }

    /// A TodoMVC-shaped React app driven end-to-end through the page actor:
    /// the live proof that controlled inputs, form submit, keyed list
    /// rendering, checkbox toggle, and delete all compose. Written with
    /// `createElement` (no JSX/Babel — that's a build step that doesn't ship
    /// to most production sites; the runtime is what we care about).
    #[test]
    #[ignore = "manual canary: needs React bundles in target/canary/"]
    fn react_todomvc_app_runs_through_the_actor() {
        let (Ok(react), Ok(react_dom)) = (
            std::fs::read("target/canary/react.production.min.js"),
            std::fs::read("target/canary/react-dom.production.min.js"),
        ) else {
            eprintln!("no React bundles in target/canary/");
            return;
        };
        const PROBE: &str = r##"
            var e = React.createElement, useState = React.useState, useRef = React.useRef;
            function TodoApp() {
                var ts = useState([]); var todos = ts[0], setTodos = ts[1];
                var ds = useState(""); var draft = ds[0], setDraft = ds[1];
                var nextId = useRef(1);
                function add(ev) {
                    ev.preventDefault();
                    var t = draft.trim(); if (!t) return;
                    setTodos(todos.concat([{ id: nextId.current++, text: t, done: false }]));
                    setDraft("");
                }
                function toggle(id) {
                    setTodos(todos.map(function (t) {
                        return t.id === id ? { id: t.id, text: t.text, done: !t.done } : t;
                    }));
                }
                function remove(id) {
                    setTodos(todos.filter(function (t) { return t.id !== id; }));
                }
                var left = todos.filter(function (t) { return !t.done; }).length;
                return e("div", { className: "todoapp" }, [
                    e("form", { key: "f", onSubmit: add }, [
                        e("input", { key: "i", id: "new-todo", value: draft,
                            onChange: function (ev) { setDraft(ev.target.value); } }),
                        e("button", { key: "a", type: "submit" }, "ADD"),
                    ]),
                    e("ul", { key: "l", className: "todo-list" }, todos.map(function (t) {
                        return e("li", { key: t.id, className: t.done ? "completed" : "active" }, [
                            e("input", { key: "c", type: "checkbox", className: "toggle",
                                checked: t.done, onChange: function () { toggle(t.id); } }),
                            e("label", { key: "t" }, t.text),
                            e("button", { key: "d", className: "destroy",
                                onClick: function () { remove(t.id); } }, "DEL-" + t.id),
                        ]);
                    })),
                    e("span", { key: "n", className: "todo-count" }, left + " left"),
                ]);
            }
            ReactDOM.createRoot(document.getElementById("target")).render(e(TodoApp));
        "##;
        let html = format!(
            "<html><head>\
             <script src=\"/react.js\"></script>\
             <script src=\"/react-dom.js\"></script>\
             </head><body><div id=\"target\"></div>\
             <script>{PROBE}</script></body></html>"
        );
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![
            ("/react.js".to_string(), Some(react)),
            ("/react-dom.js".to_string(), Some(react_dom)),
        ];
        let (handle, mut events) = spawn_page(html, env);

        // Helpers over the serialized snapshot. The actor node id of the
        // first element whose serialization (from `marker` onward) carries a
        // data-trust-node — used to find a specific control by a preceding
        // attribute like `id="new-todo"` or the `<form` tag.
        fn node_after(html: &str, marker: &str) -> usize {
            let from = html.find(marker).unwrap_or_else(|| panic!("no {marker:?}"));
            html[from..]
                .split("data-trust-node=\"")
                .nth(1)
                .and_then(|r| r.split('"').next())
                .expect("a data-trust-node")
                .parse()
                .unwrap()
        }
        // The actor node id of the clickable whose wrapped content holds `text`.
        fn marker_for(html: &str, text: &str) -> usize {
            for seg in html.split("x-trust-js:").skip(1) {
                let id: usize = seg.split(':').next().unwrap().parse().unwrap();
                let content = seg
                    .split_once("\">")
                    .map(|(_, c)| c.split("</a>").next().unwrap_or(""))
                    .unwrap_or("");
                if content.contains(text) {
                    return id;
                }
            }
            panic!("no clickable marker containing {text:?} in {html}");
        }
        let drain = |events: &mut tokio::sync::mpsc::Receiver<PageEvt>, want: &str| -> String {
            use tokio::sync::mpsc::error::TryRecvError;
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match events.try_recv() {
                    Ok(PageEvt::Updated { html, outcome })
                    | Ok(PageEvt::Static { html, outcome }) => {
                        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                        if html.contains(want) {
                            return html;
                        }
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => {
                        if Instant::now() > deadline {
                            panic!("timeout waiting for {want:?}");
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(TryRecvError::Disconnected) => panic!("actor closed waiting for {want:?}"),
                }
            }
        };

        // Adds a todo: type into the controlled input, then submit the form
        // (the ADD button is type=submit — the app routes such controls to
        // PageCmd::Submit, exactly as it would for a user pressing it).
        let add_todo = |handle: &PageHandle,
                        events: &mut tokio::sync::mpsc::Receiver<PageEvt>,
                        html: &str,
                        text: &str|
         -> String {
            let input = node_after(html, "id=\"new-todo\"");
            handle
                .cmds
                .blocking_send(PageCmd::SetValue {
                    node: input,
                    value: text.into(),
                    checked: None,
                })
                .unwrap();
            let html = drain(events, text);
            let form = node_after(&html, "<form");
            let submitter = node_after(&html, "type=\"submit\"");
            handle
                .cmds
                .blocking_send(PageCmd::Submit {
                    form,
                    submitter: Some(submitter),
                })
                .unwrap();
            html
        };

        // Initial render: empty list, "0 left".
        let html = drain(&mut events, "0 left");

        // Add the first todo and confirm it renders as an active item.
        add_todo(&handle, &mut events, &html, "buy milk");
        let html = drain(&mut events, "1 left");
        assert!(
            html.contains(">buy milk</label>"),
            "todo not rendered: {html}"
        );
        assert!(
            html.contains("class=\"active\""),
            "todo should be active: {html}"
        );

        // Add a second todo.
        add_todo(&handle, &mut events, &html, "walk dog");
        let html = drain(&mut events, "2 left");

        // Toggle the first todo complete via its checkbox.
        let check1 = node_after(&html, "class=\"toggle\"");
        handle
            .cmds
            .blocking_send(PageCmd::SetValue {
                node: check1,
                value: String::new(),
                checked: Some(true),
            })
            .unwrap();
        let html = drain(&mut events, "1 left");
        assert!(
            html.contains("class=\"completed\""),
            "checkbox toggle did not mark the todo completed: {html}"
        );

        // Delete the second todo (its destroy button reads "DEL-2").
        let del2 = marker_for(&html, "DEL-2");
        handle.cmds.blocking_send(PageCmd::Click(del2)).unwrap();
        let html = drain(&mut events, "0 left");
        assert!(!html.contains("walk dog"), "deleted todo lingered: {html}");
        assert!(html.contains("buy milk"), "wrong todo deleted: {html}");
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
    fn scan_module_imports_finds_static_skips_dynamic() {
        let src = br#"
            import a from "./a.js";
            import { b } from '/b.js';
            export { c } from "../c.js";
            import "/side-effect.js";
            import x from `./tpl.js`;
            const r = import("./dynamic.js");
            const lazy = import(`./route-${name}.js`);
            import bare from "lodash";
        "#;
        let found = scan_module_imports(src);
        // Static, path-like specifiers — including a non-interpolated
        // backtick — are caught.
        for want in ["./a.js", "/b.js", "../c.js", "/side-effect.js", "./tpl.js"] {
            assert!(found.iter().any(|s| s == want), "missing {want}: {found:?}");
        }
        // Dynamic `import(...)` is deliberately skipped (router fan-out).
        assert!(
            !found.iter().any(|s| s == "./dynamic.js"),
            "dynamic import was scanned: {found:?}"
        );
        // Interpolated template specifiers are unpredictable — skipped.
        assert!(
            !found.iter().any(|s| s.contains("route-")),
            "interpolated specifier was scanned: {found:?}"
        );
        // Bare specifiers (no path) don't resolve here — skipped.
        assert!(
            !found.iter().any(|s| s == "lodash"),
            "bare specifier was scanned: {found:?}"
        );
    }

    #[test]
    fn resize_observer_callback_receives_entries() {
        // archive.org's SharedResizeObserver does `new ResizeObserver(e =>
        // requestAnimationFrame(() => { for (let t of e) ... }))`. If our
        // RO fires the callback without a real entries array (or the value
        // doesn't survive into the rAF closure), the for-of hits
        // ToObject(undefined) — the TypeError that drove Sentry. Pin it.
        let env = PageEnv::bare("https://example.com/");
        let html = r#"<body><div id=out>no</div><script>
            const ro = new ResizeObserver(e => {
                requestAnimationFrame(() => {
                    let n = 0; for (let t of e) { n++; }
                    document.getElementById('out').textContent = 'entries=' + n;
                });
            });
            ro.observe(document.body);
        </script></body>"#;
        let (out, outcome) = transform(html, &env);
        assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
        assert!(out.contains("entries=1"), "RO entries not delivered: {out}");
    }

    #[test]
    fn this_in_doubly_nested_arrow_survives_capture() {
        // Minimal pin for the Boa `this`-escape fix (the archive.org /
        // SharedResizeObserver crash, distilled). A non-arrow function whose
        // ONLY reason to materialize a function environment is a `this`
        // referenced from a DOUBLY-nested arrow. The old escape analysis
        // marked the intermediate arrow scope (arrows never materialize for
        // `this`), so the enclosing function stayed un-materialized and `this`
        // resolved to the wrong (outer) environment — `f()` returned
        // undefined. No module/ResizeObserver/Sentry needed: this is pure
        // engine scoping.
        let env = PageEnv::bare("https://example.com/");
        let (out, outcome) = transform(
            r#"<body><div id=out>no</div><script>
                function Outer() {
                    this.v = 42;
                    this.make = () => () => this.v;
                }
                document.getElementById('out').textContent =
                    'val ' + (new Outer().make()());
            </script></body>"#,
            &env,
        );
        assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
        assert!(out.contains("val 42"), "this lost in nested arrow: {out}");
    }

    // Regression (2026-06-16): archive.org's `SharedResizeObserver`, wrapped by
    // Sentry's `Ti` function-instrumentation, threw "cannot convert undefined
    // to object". Faithful shape: a MODULE with `var t=class{…},n=e(…)`, a
    // `new ResizeObserver(e => rAF(() => { for (let t of e) this.…(t.target) }))`,
    // and Sentry's `Ti` wrapping of setTimeout/rAF. Root cause was NOT the
    // captured `e` (it resolves fine) but the inner arrow's `this`: a `this`
    // read from a doubly-nested arrow escaped onto the intermediate arrow scope
    // instead of the class constructor, so the constructor never materialized
    // and `this.resizeHandlers` was a property access on `undefined`. Fixed in
    // the Boa fork's scope analyzer (arrow scopes are skipped when escaping
    // `this`). See `this_in_doubly_nested_arrow_survives_capture` for the
    // distilled case. Librewolf: 0 errors.
    #[test]
    fn sentry_wrapped_resize_observer_keeps_captured_entries() {
        let env = PageEnv::bare("https://example.com/");
        env.cache.seed(
            String::from("https://example.com/lib.js"),
            200,
            String::from("text/javascript"),
            b"export const n = (o) => o;".to_vec(),
        );
        env.cache.seed(
            String::from("https://example.com/main.js"),
            200,
            String::from("text/javascript"),
            br#"import { n as e } from './lib.js';
                function Ti(fn){
                    if (typeof fn !== 'function') return fn;
                    if (fn.__sentry_wrapped__) return fn.__sentry_wrapped__;
                    const r = function(){ const m = Array.prototype.map.call(arguments, x => Ti(x)); return fn.apply(this, m); };
                    try { for (let k in fn) if (Object.prototype.hasOwnProperty.call(fn,k)) r[k]=fn[k]; } catch(_){}
                    try { Object.defineProperty(fn, '__sentry_wrapped__', { value: r, configurable: true }); } catch(_){}
                    return r;
                }
                function fill(o,n,repl){ o[n]=repl(o[n]); }
                fill(globalThis,'requestAnimationFrame', o => function(cb){ return o.apply(this,[Ti(cb)]); });
                fill(globalThis,'setTimeout', o => function(){ arguments[0]=Ti(arguments[0]); return o.apply(this,arguments); });
                var t = class {
                    constructor() {
                        this.resizeObserver = new ResizeObserver(e => {
                            window.requestAnimationFrame(() => {
                                for (let t of e) this.resizeHandlers.get(t.target);
                                document.getElementById('out').textContent = 'looped ' + e.length;
                            });
                        });
                        this.resizeHandlers = new Map();
                    }
                    add(el) { this.resizeHandlers.set(el, 1); this.resizeObserver.observe(el); }
                }, n = e({ SharedResizeObserver: () => t });
                new t().add(document.body);"#
                .to_vec(),
        );
        let (out, outcome) = transform(
            "<body><div id=out>no</div>\
             <script type=module src='/main.js'></script></body>",
            &env,
        );
        assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
        assert!(out.contains("looped 1"), "captured RO entries lost: {out}");
    }

    #[test]
    fn preloaded_modules_run_without_network() {
        // The parallel-prefetch contract: bodies seeded into the shared
        // cache serve the module graph — entry AND imports — with no net
        // grant at all.
        let env = PageEnv::bare("https://example.com/");
        env.cache.seed(
            String::from("https://example.com/main.js"),
            200,
            String::from("text/javascript"),
            b"import { x } from './lib.js';\
              document.getElementById('t').textContent = x;"
                .to_vec(),
        );
        env.cache.seed(
            String::from("https://example.com/lib.js"),
            200,
            String::from("text/javascript"),
            b"export const x = 'preloaded graph';".to_vec(),
        );
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
    fn wide_module_fanout_runs_instead_of_falling_back() {
        // Regression for the removed import/preload caps: an entry module
        // that statically imports 30 modules (a 31-preload graph) used to be
        // SKIPPED wholesale (`MAX_SAFE_MODULE_IMPORTS`/`MAX_SAFE_MODULE_PRELOADS`
        // = 24) and the page fell back to un-transformed HTML — the blunt
        // workaround for Boa's cyclic-module crash. With the Boa fork making
        // that crash a catchable panic instead, a wide *acyclic* graph like
        // this must just run.
        const N: usize = 30;
        let env = PageEnv::bare("https://example.com/");
        let mut imports = String::new();
        let mut sum = String::new();
        for i in 0..N {
            imports.push_str(&format!("import {{ v as v{i} }} from './m{i}.js';\n"));
            if i > 0 {
                sum.push('+');
            }
            sum.push_str(&format!("v{i}"));
            env.cache.seed(
                format!("https://example.com/m{i}.js"),
                200,
                String::from("text/javascript"),
                format!("export const v = {i};").into_bytes(),
            );
        }
        env.cache.seed(
            String::from("https://example.com/main.js"),
            200,
            String::from("text/javascript"),
            format!("{imports}document.getElementById('t').textContent = 'sum=' + ({sum});")
                .into_bytes(),
        );

        let (out, outcome) = transform(
            "<body><div id=t></div>\
             <script type=module src='/main.js'></script></body>",
            &env,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.modules_skipped, 0);
        // 0 + 1 + ... + 29 = 435.
        assert!(out.contains("sum=435"), "{out}");
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
    fn live_click_moves_active_tab_border_via_cascade_rebake() {
        // A tab UI on a living page: clicking the inactive tab moves the
        // `selected` class, and the re-serialization RE-BAKES the cascade so
        // the active border follows — the new tab gains `border-bottom:4px`,
        // the old one drops to `1px`. This is the SL Marketplace tab case.
        let (handle, mut events) = live(
            "<head><style>.tab{border-bottom:1px solid}.tab.selected{border-bottom:4px solid}</style></head>\
             <body>\
             <a href=\"#\" class=tablink><div class=\"tab selected\" id=t1>Items</div></a>\
             <a href=\"#\" class=tablink><div class=tab id=t2>Merchants</div></a>\
             <script>\
             document.querySelectorAll('.tablink').forEach(function(a){\
               a.addEventListener('click', function(e){\
                 e.preventDefault();\
                 document.querySelectorAll('.tab').forEach(function(d){d.classList.remove('selected');});\
                 a.querySelector('.tab').classList.add('selected');\
               });\
             });</script></body>",
        );
        // The baked border-bottom width on the tab div with the given id.
        let border_of = |html: &str, id: &str| -> String {
            let at = html.find(&format!("id=\"{id}\"")).expect("tab present");
            let style_at = html[at..].find("style=\"").expect("baked style") + at + 7;
            let end = html[style_at..].find('"').unwrap() + style_at;
            html[style_at..end]
                .split(';')
                .find_map(|d| {
                    d.trim()
                        .strip_prefix("border-bottom-width:")
                        .map(str::to_string)
                })
                .unwrap_or_default()
        };
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("expected first Updated");
        };
        assert_eq!(border_of(&html, "t1"), "4px", "Items starts active");
        assert_eq!(border_of(&html, "t2"), "1px", "Merchants starts inactive");

        let at = html.find("Merchants").unwrap();
        let marker = html[..at]
            .rfind("x-trust-js:")
            .map(|i| {
                html[i + "x-trust-js:".len()..]
                    .split(':')
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
            .unwrap();
        handle.cmds.blocking_send(PageCmd::Click(marker)).unwrap();
        let Some(PageEvt::Updated { html, outcome }) = events.blocking_recv() else {
            panic!("expected Updated after click");
        };
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(
            border_of(&html, "t1"),
            "1px",
            "Items deactivated after click"
        );
        assert_eq!(border_of(&html, "t2"), "4px", "Merchants now active");
    }

    fn live_node_after(html: &str, marker: &str) -> usize {
        let start = html.find(marker).expect("marker in live html");
        let rest = &html[start..];
        let attr = "data-trust-node=\"";
        let attr_start = rest.find(attr).expect("live node attr") + attr.len();
        let rest = &rest[attr_start..];
        let attr_end = rest.find('"').expect("attr end");
        rest[..attr_end].parse().expect("node id")
    }

    #[test]
    fn live_form_input_sets_dom_and_fires_input_change() {
        let (handle, mut events) = live(
            "<body><form><input name=msg><p id=out></p></form>\
             <script>\
             const msg = document.querySelector('input');\
             const out = document.getElementById('out');\
             const log = [];\
             msg.addEventListener('input', function () { log.push('input:' + msg.value); });\
             msg.addEventListener('change', function () { log.push('change:' + msg.value); out.textContent = log.join('|'); });\
             </script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("live form should keep the actor resident");
        };
        let input = live_node_after(&html, "name=\"msg\"");
        handle
            .cmds
            .blocking_send(PageCmd::SetValue {
                node: input,
                value: String::from("hello"),
                checked: None,
            })
            .unwrap();
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("form edit should render an update");
        };
        assert!(html.contains("value=\"hello\""), "{html}");
        assert!(
            html.contains("input:hello|change:hello"),
            "input/change fired in order: {html}"
        );
    }

    #[test]
    fn live_form_submit_prevent_default_updates_in_place() {
        let (handle, mut events) = live(
            "<body><form><input name=msg value=hi><button type=submit value=go>Send</button><p id=out></p></form>\
             <script>\
             const form = document.querySelector('form');\
             form.addEventListener('submit', function (event) {\
               event.preventDefault();\
               document.getElementById('out').textContent = 'submit:' + form.querySelector('input').value + ':' + event.submitter.value;\
             });\
             </script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("live form should keep the actor resident");
        };
        let form = live_node_after(&html, "<form");
        let button = live_node_after(&html, "<button");
        handle
            .cmds
            .blocking_send(PageCmd::Submit {
                form,
                submitter: Some(button),
            })
            .unwrap();
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("prevented submit should update in place");
        };
        assert!(html.contains("submit:hi:go"), "{html}");
    }

    #[test]
    fn live_form_submit_default_falls_back_to_app() {
        let (handle, mut events) = live(
            "<body><form><input name=msg value=hi><button type=submit>Send</button></form><script>document.body.dataset.ready='1';</script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("live form should keep the actor resident");
        };
        let form = live_node_after(&html, "<form");
        let button = live_node_after(&html, "<button");
        handle
            .cmds
            .blocking_send(PageCmd::Submit {
                form,
                submitter: Some(button),
            })
            .unwrap();
        match events.blocking_recv() {
            Some(PageEvt::SubmitDefault) => {}
            other => panic!("unprevented submit should ask app to submit, got {other:?}"),
        }
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

        // Dispatch the no-op first: it acknowledges completion without
        // an Updated render, so the dirty bit still held.
        handle.cmds.blocking_send(PageCmd::Click(noop)).unwrap();
        match events.blocking_recv() {
            Some(PageEvt::Settled) => {}
            other => panic!("no-op click should only settle, got {other:?}"),
        }
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
    fn script_location_assign_navigates_after_click() {
        let (handle, mut events) = live(
            r##"<body><button id=go onclick="location.assign('/script-next?x=1')">go</button>
             <script>void 0;</script></body>"##,
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
            Some(PageEvt::Navigate(url)) => {
                assert_eq!(url, "https://example.com/script-next?x=1");
            }
            other => panic!("expected script Navigate, got {other:?}"),
        }
    }

    #[test]
    fn script_location_href_assignment_navigates_after_form_input() {
        let (handle, mut events) = live(
            r##"<body><form><input name=q></form><script>
             document.querySelector('input').addEventListener('change', function () {
               window.location.href = '../changed';
             });</script></body>"##,
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("live form should keep the actor resident");
        };
        let input = live_node_after(&html, "name=\"q\"");
        handle
            .cmds
            .blocking_send(PageCmd::SetValue {
                node: input,
                value: String::from("go"),
                checked: None,
            })
            .unwrap();
        match events.blocking_recv() {
            Some(PageEvt::Navigate(url)) => {
                assert_eq!(url, "https://example.com/changed");
            }
            other => panic!("expected script Navigate, got {other:?}"),
        }
    }

    #[test]
    fn same_document_hash_navigation_stays_live_and_fires_hashchange() {
        let (handle, mut events) = live(
            r##"<body><button id=go onclick="location.hash = 'route'">route</button>
             <p id=out></p><script>
             window.addEventListener('hashchange', function (event) {
               document.getElementById('out').textContent = location.hash + '|' + event.oldURL + '>' + event.newURL;
             });</script></body>"##,
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
            Some(PageEvt::Updated { html, .. }) => {
                assert!(
                    html.contains(
                        "#route|https://example.com/dir/page&gt;https://example.com/dir/page#route"
                    ),
                    "{html}"
                );
            }
            other => panic!("hash route should update in place, got {other:?}"),
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
    fn bare_specifier_imports_reject_without_recursive_formatting() {
        let (out, outcome) = page(
            "<body><p>kept</p><script type=module>\
             import {LitElement} from 'lit';\
             </script></body>",
        );
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        // Real rejection reason now (was a generic "module rejected" while
        // the cyclic-module `Debug` recursion made formatting dangerous).
        // Reaching this assert at all proves formatting didn't blow up.
        assert!(
            outcome.errors[0].contains("cannot resolve module specifier")
                && outcome.errors[0].contains("lit"),
            "{:?}",
            outcome.errors
        );
        assert!(out.contains("kept"), "{out}");
    }

    #[test]
    fn thrown_errors_carry_a_js_readable_stack() {
        // Boa captures a backtrace on every throw (backtrace_limit 50) and
        // its Display renders it, but materializing the JS error object used
        // to drop it — page JS saw no `.stack`. The fork surfaces it at the
        // `JsError::to_opaque` boundary that both catch and rejection use.
        let (out, outcome) = page(
            "<body><pre id=out></pre><script>\
             function inner() { throw new TypeError('boom'); }\
             function outer() { inner(); }\
             try { outer(); } catch (e) { \
               document.getElementById('out').textContent = e.stack || 'NO STACK'; }\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(!out.contains("NO STACK"), "stack was empty: {out}");
        assert!(out.contains("boom"), "{out}");
        // The new capability: real call-frame lines, V8-style.
        assert!(out.contains("    at "), "no stack frames: {out}");
    }

    #[test]
    fn class_constructor_const_captured_by_closure_does_not_panic() {
        // Trap #6 (Boa fork fix): a `const`/`let` in a class-constructor body
        // captured by a closure used to abort the VM. The constructor pushed a
        // function scope UNCONDITIONALLY, but boa_parser assigns binding
        // scope-indices assuming an all-local one is elided — so the captured
        // const resolved to the empty function env (0 slots) and the define
        // opcode wrote out of bounds. Real pages (archive.org home, danbooru
        // tiles, Lit) trip exactly this. The fix makes the constructor honor
        // the same conditional-push rule regular functions use.
        let (out, outcome) = page(
            "<body><pre id=out></pre><script>\
             class C { constructor() { const x = 7; this.cmp = (a,b) => (a-b) + (x-x); } }\
             var arr=[3,1,2]; arr.sort(new C().cmp);\
             document.getElementById('out').textContent = 'sorted=' + arr.join(',');\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(!outcome.panicked, "engine panicked (trap #6 regressed)");
        // Correct VALUE, not just non-panic: x-x=0 keeps the numeric sort.
        assert!(out.contains("sorted=1,2,3"), "{out}");
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
    fn a_custom_element_defined_after_insertion_into_shadow_upgrades() {
        // archive.org's router renders the page component into a shadow
        // tree BEFORE its module defines it; define()'s catch-up upgrade
        // must pierce shadow roots (document.querySelectorAll does not),
        // or the element never constructs and the page stays an empty
        // shell. The connectedCallback writing a LIGHT-dom node proves it
        // upgraded AND connected through the shadow boundary.
        let (out, outcome) = page(
            "<body><div id=host></div><p id=result>NOT</p><script>\
             const host = document.getElementById('host');\
             const sr = host.attachShadow({ mode: 'open' });\
             sr.innerHTML = '<late-el></late-el>';\
             class LateEl extends HTMLElement {\
               connectedCallback() { document.getElementById('result').textContent = 'UPGRADED'; }\
             }\
             customElements.define('late-el', LateEl);\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains(">UPGRADED<"), "{out}");
    }

    #[test]
    fn crypto_subtle_digest_and_random_work() {
        // Real SHA-1/SHA-256 (libraries hash request ids before they
        // fetch — archive.org's collection search gates its tile fetch
        // on a SHA-1 uid) plus functional getRandomValues/randomUUID.
        let (out, outcome) = page(
            "<body><p id=s1></p><p id=s256></p><p id=rng></p><script>\
             const hex = (b) => [...new Uint8Array(b)].map(x=>x.toString(16).padStart(2,'0')).join('');\
             const enc = new TextEncoder();\
             Promise.all([\
               crypto.subtle.digest('SHA-1', enc.encode('abc')).then(b=>{document.getElementById('s1').textContent=hex(b);}),\
               crypto.subtle.digest('SHA-256', enc.encode('abc')).then(b=>{document.getElementById('s256').textContent=hex(b);}),\
             ]);\
             const u = crypto.randomUUID();\
             const r = crypto.getRandomValues(new Uint8Array(8));\
             document.getElementById('rng').textContent =\
               (u.length === 36 && u[14] === '4' && r.length === 8 && typeof r[0] === 'number') ? 'ok' : 'bad';\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("a9993e364706816aba3e25717850c26c9cd0d89d"),
            "sha1: {out}"
        );
        assert!(
            out.contains("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            "sha256: {out}"
        );
        assert!(out.contains(">ok<"), "rng: {out}");
    }

    #[test]
    fn dom_exception_is_a_real_constructor() {
        // core-js's DOMException polyfill reads getBuiltIn("DOMException")
        // .prototype during detection; undefined → ToObject throw that
        // tore down danbooru's init. It must be a real Error subclass.
        let (out, outcome) = page(
            "<body><p id=o></p><script>\
             const e = new DOMException('boom', 'NotFoundError');\
             document.getElementById('o').textContent = [\
               e instanceof Error, e.name, e.message, e.code,\
               DOMException.NOT_FOUND_ERR, Object.prototype.toString.call(e)\
             ].join('|');\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("true|NotFoundError|boom|8|8|[object DOMException]"),
            "{out}"
        );
    }

    #[test]
    fn host_objects_are_tagged_not_plain() {
        // window/document must report a host tag, else a deep-merge/clone
        // (jQuery UI widget.extend via isPlainObject) follows window.window
        // / document.defaultView in an infinite cycle (broke danbooru).
        let (out, outcome) = page(
            "<body><p id=o></p><script>\
             const t = (x) => Object.prototype.toString.call(x);\
             document.getElementById('o').textContent =\
               [t(window), t(document), t({})].join('|');\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("[object Window]|[object HTMLDocument]|[object Object]"),
            "{out}"
        );
    }

    #[test]
    fn element_attributes_is_an_array_like_named_node_map() {
        // Alpine.js morphs the DOM with `Array.from(el.attributes)`;
        // a missing `attributes` made that `Array.from(undefined)` →
        // ToObject throw that aborted danbooru's post-grid render.
        let (out, outcome) = page(
            "<body><div id=root title=hi data-x=7></div><p id=o></p><script>\
             const d = document.getElementById('root');\
             const attrs = Array.from(d.attributes);\
             const by = (n) => attrs.find((a) => a.name === n);\
             document.getElementById('o').textContent = [\
               attrs.length, d.attributes.length, by('data-x').value,\
               by('title').value, by('id').value, d.attributes.getNamedItem('id').value\
             ].join('|');\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains(">3|3|7|hi|root|root<"), "{out}");
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
    fn get_computed_style_reads_the_cascade_not_just_inline() {
        // getComputedStyle now resolves sheet rules, inheritance, and UA
        // defaults via the cascade — it used to see only the inline style
        // attribute, returning "" for everything a stylesheet set.
        let (out, outcome) = page(
            "<head><style>.card{font-weight:bold}</style></head>\
             <body><div class=card><span id=s>x</span></div>\
             <b id=b>y</b>\
             <script>\
               var s = document.getElementById('s');\
               var b = document.getElementById('b');\
               document.body.setAttribute('data-inherit', getComputedStyle(s).fontWeight);\
               document.body.setAttribute('data-ua', getComputedStyle(b).getPropertyValue('font-weight'));\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("data-inherit=\"bold\""),
            "sheet font-weight inherits through getComputedStyle: {out}"
        );
        assert!(
            out.contains("data-ua=\"bold\""),
            "UA <b> default visible through getComputedStyle: {out}"
        );
    }

    #[test]
    fn media_queries_apply_at_the_viewport_width() {
        // bare() viewport = 80 cols × 8px = 640px wide. A max-width:768px
        // block matches (hides); a min-width:1000px block doesn't (kept).
        let (out, outcome) = page(
            "<head><style>\
               @media (max-width: 768px) { .mobile-hide { display: none } }\
               @media (min-width: 1000px) { .desktop-hide { display: none } }\
             </style></head>\
             <body>\
               <p class=mobile-hide>HIDE_AT_NARROW</p>\
               <p class=desktop-hide>KEEP_AT_NARROW</p>\
               <script>document.title = 'x';</script>\
             </body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            !out.contains("HIDE_AT_NARROW"),
            "max-width:768px matched at 640px → hidden: {out}"
        );
        assert!(
            out.contains("KEEP_AT_NARROW"),
            "min-width:1000px not matched at 640px → kept: {out}"
        );
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
