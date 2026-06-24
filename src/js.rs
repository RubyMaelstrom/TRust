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

    /// Push the deadline out to at least `now + dur`, never pulling it in. A
    /// dispatch-time network request calls this (see `PageNet::dispatch`) so a
    /// slow wire — an LLM completion — can finish instead of being cancelled at
    /// the tight compute deadline; a load's already-distant deadline is untouched.
    pub fn extend_at_least(&self, dur: Duration) {
        let target = Instant::now() + dur;
        if target > self.deadline.get() {
            self.deadline.set(target);
        }
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
            // The phase profiler times THIS synchronous drain as "execute" —
            // the `group.next().await` park below (network wait) is left out,
            // so the bucket is real VM CPU, not wire time.
            let exec_t = phase_begin();
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
            phase_end(Phase::Execute, exec_t);
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

// ---- Per-phase load profiler (Step 1 decision-gate instrumentation) ----
//
// Decompose a LIVE page load into parse / compile / execute wall time,
// accumulated across every script, module, and job-drain on the page thread.
// The `engine_profile` test times one bundle in isolation; it CANNOT capture
// the settle-time execution where a real SPA spends its seconds (Steam's
// ~15s tail), which only exists in the full page context. This thread-local
// accumulator runs across the whole live load instead — the tool the JS
// engine performance plan's Step 1 gate needs to decide whether parse+compile
// or execution dominates a real rendered page (and thus whether lazy compile
// is the prize or execution work must be reopened).
//
// Thread-local because all of a page's JS runs on its own `trust-page` thread
// (or the calling thread for the one-shot `transform`). Reset at `load_page`
// start, reported at `settle_page` end, gated on `TRUST_JS_PHASE` (each thread
// reads the env var once when its accumulator is first touched, so the
// `trust-page` thread honors a process-wide `TRUST_JS_PHASE=1`).
//
// Zero behaviour change: `run_script` now splits `ctx.eval` into its exact
// constituent steps — `Script::parse` → `Script::codeblock` → `Script::evaluate`
// — which is what `ctx.eval` does internally (`eval` == `parse?.evaluate`, and
// `evaluate`'s `prepare_run` memoizes `codeblock`; see vendored
// `Context::eval`/`script.rs`). The split is unconditional so what ships is
// what we measure; the `Instant::now()` calls are gated so the unprofiled path
// pays only a cached bool check.
//
// CAVEATS (read before interpreting numbers): (1) "execute" is the synchronous
// JS the VM runs — classic-script top level PLUS every synchronous job/microtask
// drained by the executor — but it EXCLUDES the network parking the executor
// sleeps on (that wall lands in `load wall − measured CPU`). (2) ES-module
// compilation happens inside Boa's `load_link_evaluate`/link (not separately
// exposed), so a module's compile time is attributed to "execute", not
// "compile"; the decision-gate page (Steam) is classic-script-dominated so this
// is benign, but a module-dominated page's "compile" is a lower bound. (3) A
// handful of internal `__trust.*` control evals (tick/scanImageLoads) are
// negligible and left unmeasured. (4) With parallel parse (Step 5a) ON, the
// raw parse of external classic scripts runs on worker threads, so its CPU is
// NOT in this (page-thread) accumulator — "parse" here drops toward zero and
// `measured JS CPU` falls below `/usr/bin/time` User (which sums all threads).
// That gap IS the parallel-parse win; read wall, not the per-phase split, to
// size it.

#[derive(Default, Clone, Copy, Debug)]
pub struct PhaseProfile {
    pub parse: Duration,
    pub parse_n: u32,
    pub compile: Duration,
    pub compile_n: u32,
    pub execute: Duration,
    pub execute_n: u32,
}

#[derive(Clone, Copy)]
enum Phase {
    Parse,
    Compile,
    Execute,
}

thread_local! {
    static PHASES: RefCell<PhaseProfile> = const { RefCell::new(PhaseProfile {
        parse: Duration::ZERO, parse_n: 0,
        compile: Duration::ZERO, compile_n: 0,
        execute: Duration::ZERO, execute_n: 0,
    }) };
    // Per-thread arm state, seeded from the process-wide env flag (so the
    // `trust-page` thread honors `TRUST_JS_PHASE`). Seeding reads the global
    // `OnceLock` — NOT `env::var_os` — so the env (and its global lock) is
    // scanned exactly once per process, never per thread. A test overrides it
    // on its own thread via `phases_arm`.
    static PHASE_ARMED: std::cell::Cell<bool> = std::cell::Cell::new(phase_env_default());
}

/// The process-wide `TRUST_JS_PHASE` flag, read from the environment exactly
/// once. Keeping the env scan out of the per-thread seed keeps the hot path a
/// single thread-local `Cell` read with no env-lock traffic.
fn phase_env_default() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TRUST_JS_PHASE").is_some())
}

#[inline]
fn phases_on() -> bool {
    PHASE_ARMED.with(std::cell::Cell::get)
}

/// Begin a `Some(Instant)` if profiling, else `None` — pairs with `phase_end`.
#[inline]
fn phase_begin() -> Option<Instant> {
    phases_on().then(Instant::now)
}

/// Accumulate `t.elapsed()` into `phase`, if a timer was started.
#[inline]
fn phase_end(phase: Phase, t: Option<Instant>) {
    let Some(t) = t else { return };
    let d = t.elapsed();
    PHASES.with(|p| {
        let mut p = p.borrow_mut();
        match phase {
            Phase::Parse => {
                p.parse += d;
                p.parse_n += 1;
            }
            Phase::Compile => {
                p.compile += d;
                p.compile_n += 1;
            }
            Phase::Execute => {
                p.execute += d;
                p.execute_n += 1;
            }
        }
    });
}

/// Zero the accumulator — called at the start of every page load.
fn phases_reset() {
    PHASES.with(|p| *p.borrow_mut() = PhaseProfile::default());
}

/// Read the accumulator without clearing it (the next load's `phases_reset`
/// clears it). Lets the `settle_page` report and a test read the same numbers.
fn phases_snapshot() -> PhaseProfile {
    PHASES.with(|p| *p.borrow())
}

/// Force-arm/disarm profiling on the current thread (tests; production reads
/// the env var per thread).
#[cfg(test)]
fn phases_arm(on: bool) {
    PHASE_ARMED.with(|c| c.set(on));
}

/// Print the per-phase decomposition of a finished live load (under
/// `TRUST_JS_PHASE`). `wall` is the full load+settle wall clock. The gate
/// reading: parse+compile is the ceiling lazy compilation can remove; if it is
/// a small share of the measured JS CPU (cross-check against `/usr/bin/time -v`
/// User CPU, which should be ≈ the CPU line here), execution dominates and the
/// plan's execution-throughput removal must be reopened.
fn report_phases(wall: Duration) {
    if !phases_on() {
        return;
    }
    let p = phases_snapshot();
    let cpu = p.parse + p.compile + p.execute;
    let pc = p.parse + p.compile;
    let pct = |a: Duration, b: Duration| {
        if b.is_zero() {
            0.0
        } else {
            100.0 * a.as_secs_f64() / b.as_secs_f64()
        }
    };
    let at = crate::http::trace_ms();
    eprintln!("js : @{at:>6}ms ─── phase profile (whole live load) ───");
    eprintln!(
        "       parse   : {:>10.3?}  ({} units, {:>4.1}% of JS CPU)",
        p.parse,
        p.parse_n,
        pct(p.parse, cpu)
    );
    eprintln!(
        "       compile : {:>10.3?}  ({} units, {:>4.1}% of JS CPU)",
        p.compile,
        p.compile_n,
        pct(p.compile, cpu)
    );
    eprintln!(
        "       execute : {:>10.3?}  ({} drains+toplevel, {:>4.1}% of JS CPU; incl. module compile)",
        p.execute,
        p.execute_n,
        pct(p.execute, cpu)
    );
    eprintln!(
        "       ── measured JS CPU (parse+compile+execute): {cpu:.3?}  [≈ /usr/bin/time User]"
    );
    eprintln!(
        "       ── parse+compile = {:.3?} ({:>4.1}% of JS CPU)  ← lazy-compile lever ceiling",
        pc,
        pct(pc, cpu)
    );
    eprintln!(
        "       ── load wall: {:.3?}  (wall − JS CPU ≈ network wait + Rust-side: {:.3?})",
        wall,
        wall.saturating_sub(cpu)
    );
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
        // Tag the source with its URL/label so backtraces name the file
        // (classic scripts were anonymous "unknown" — modules already do this).
        //
        // This is `ctx.eval` unrolled into its exact three steps so the phase
        // profiler can time each: `eval` IS `Script::parse(..)?.evaluate(..)`,
        // and `evaluate`'s `prepare_run` memoizes `codeblock` — so pre-calling
        // `codeblock` only lets us time compilation separately; the result is
        // byte-for-byte identical to `ctx.eval`.
        let src = Source::from_bytes(source).with_path(std::path::Path::new(name));
        let t = phase_begin();
        let script = boa_engine::Script::parse(src, None, ctx)?;
        phase_end(Phase::Parse, t);
        let t = phase_begin();
        script.codeblock(ctx)?;
        phase_end(Phase::Compile, t);
        let t = phase_begin();
        let r = script.evaluate(ctx);
        phase_end(Phase::Execute, t);
        r
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

/// Process-global detached image of the compiled PRELUDE — the in-memory
/// compiled-code cache's first consumer (JS-engine performance plan, Step 4,
/// built on the K1/K2 keystone `CodeBlockImage`).
///
/// The prelude is the same ~65 KB of JS on every page, and its parse+compile is
/// the bulk of the per-page prelude cost. A compiled `CodeBlock` is normally
/// pinned to the thread + GC heap that built it, but a `CodeBlockImage` is the
/// detached, `Send + Sync`, `Gc`-free form: we compile the prelude ONCE per
/// process, store its image here, and rehydrate a fresh `Gc<CodeBlock>` into
/// each page thread's own heap — no re-parse, no re-compile.
///
/// Sound to reuse across realms because the prelude is a single top-level IIFE:
/// it declares NO top-level `let`/`const`/`function`/`var` (every global is a
/// `globalThis.X = …` property write from inside the IIFE), so its compiled
/// `<main>` block references no realm global-binding slots and runs identically
/// in any realm — exactly the cross-realm path the keystone tests exercise. It
/// also has no dynamic `import()` (the only runtime use of the carrier script's
/// own AST), so the empty shell it is installed into is never dereferenced.
static PRELUDE_IMAGE: std::sync::OnceLock<boa_engine::vm::CodeBlockImage> =
    std::sync::OnceLock::new();

/// Run a rehydrated [`CodeBlockImage`] as a script in `ctx`'s realm: install it
/// into an empty carrier script (`Script::parse("")`, ~free, which binds the
/// carrier to this realm) and evaluate. The shared execution seam behind both
/// the prelude cache (Step 4) and the cross-page CDN cache (Phase 2) — see the
/// JS-engine performance plan.
///
/// Sound ONLY for a realm-portable image: one whose source declared no globals,
/// so its compiled `<main>` block references no realm global-binding slots and
/// has no global-declaration side effects (the prelude has this by construction;
/// CDN libraries are admitted only after the realm-portability gate). The empty
/// carrier is never dereferenced as a dynamic-`import()` referrer because such a
/// block has no dynamic import. `name` (the script URL or "prelude") becomes the
/// carrier's source path so a thrown error's backtrace still names the file.
///
/// Same budget gate, `catch_unwind`, and error/panic accounting as
/// [`run_script`], so a (hypothetical) failure degrades the page identically.
fn run_rehydrated_image(
    ctx: &mut Context,
    name: &str,
    image: &boa_engine::vm::CodeBlockImage,
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
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let src = Source::from_bytes("".as_bytes()).with_path(std::path::Path::new(name));
        let t = phase_begin();
        let shell = boa_engine::Script::parse(src, None, ctx)?;
        phase_end(Phase::Parse, t);
        let t = phase_begin();
        shell.set_codeblock(boa_engine::vm::CodeBlock::from_image(image));
        phase_end(Phase::Compile, t);
        let t = phase_begin();
        let r = shell.evaluate(ctx);
        phase_end(Phase::Execute, t);
        r
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

/// Run the PRELUDE through the process-global compiled-code cache: the first
/// page parses + compiles it and stores the detached image; every later page
/// (on its own `trust-js`/`trust-page` thread) rehydrates that image and skips
/// both the 65 KB parse and the compile, paying only execution — which every
/// realm must pay regardless, since the prelude's globals are installed by
/// *running* the IIFE, not by declaring them.
///
/// The parse-skipping twin of [`run_script`] for our one always-present,
/// always-identical script: same budget gate, `catch_unwind`, and error/panic
/// accounting, so a (hypothetical) prelude failure degrades the page exactly as
/// before. `TRUST_NO_PRELUDE_CACHE` forces the cold `run_script` path — the A/B
/// toggle and safety valve, like `TRUST_NO_PARALLEL_PARSE` for parallel parse.
fn run_prelude(ctx: &mut Context, budget: &Budget, outcome: &mut Outcome) {
    if std::env::var_os("TRUST_NO_PRELUDE_CACHE").is_some() {
        run_script(ctx, "prelude", PRELUDE.as_bytes(), budget, outcome);
        return;
    }
    if let Some(image) = PRELUDE_IMAGE.get() {
        // Cache hit: rehydrate the compiled block into THIS thread's heap and
        // run it via an empty carrier — no 225 KB parse, no compile.
        run_rehydrated_image(ctx, "prelude", image, budget, outcome);
        return;
    }
    if budget.exhausted() || outcome.elapsed >= COMPUTE_BUDGET {
        outcome
            .errors
            .push(String::from("prelude: skipped, page JS budget exhausted"));
        return;
    }
    let started = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Cold (first page of the process, or a rare concurrent-first race):
        // parse + compile the real prelude, cache its detached image for later
        // pages, then run. A lost `set` race is harmless — the winner's image
        // serves the cache and our own freshly compiled block still runs this
        // page (`let _ =` discards the rejected value).
        let src = Source::from_bytes(PRELUDE.as_bytes()).with_path(std::path::Path::new("prelude"));
        let t = phase_begin();
        let script = boa_engine::Script::parse(src, None, ctx)?;
        phase_end(Phase::Parse, t);
        let t = phase_begin();
        let cb = script.codeblock(ctx)?;
        let _ = PRELUDE_IMAGE.set(cb.to_image());
        phase_end(Phase::Compile, t);
        let t = phase_begin();
        let r = script.evaluate(ctx);
        phase_end(Phase::Execute, t);
        r
    }));
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => outcome.errors.push(format!("prelude: {err}")),
        Err(_) => {
            outcome.errors.push(String::from(
                "prelude: engine panic (Boa bug) — page JS halted",
            ));
            outcome.panicked = true;
        }
    }
    outcome.elapsed += started.elapsed();
}

/// Max parse workers in the parallel-parse pool (Step 5a of the JS-engine
/// performance plan). Bounded like `PREFETCH_CONCURRENCY` so a page with
/// hundreds of `<script>`s can't spawn a thread storm; clamped to core count
/// and job count below.
const PARSE_CONCURRENCY: usize = 8;

/// The raw-parse half of parallel parse. External classic scripts are lexed and
/// parsed to ASTs on a pool of big-stack worker threads — OFF the page thread —
/// while the page thread runs the prelude and earlier scripts. Scope analysis
/// and bytecode compilation stay on the page thread, in document order, because
/// global lexical bindings (`let`/`const`/`class`) resolve at runtime by slot
/// index into one shared global environment, so their declaration order must be
/// preserved (see `boa_engine::Script::compile_raw`). Each worker parses with a
/// private interner that rides along with its AST; no interner merge is needed
/// because the shared scope links scripts by `JsString` name, not interner index.
struct ParsePool {
    rx: std::sync::mpsc::Receiver<(usize, Result<boa_engine::RawScript, String>)>,
    buffered: std::collections::HashMap<usize, Result<boa_engine::RawScript, String>>,
    dispatched: std::collections::HashSet<usize>,
}

impl ParsePool {
    /// Whether script `index` went to the pool (the rest are parsed inline).
    fn was_dispatched(&self, index: usize) -> bool {
        self.dispatched.contains(&index)
    }

    /// Take script `index`'s raw parse, buffering any other results that arrive
    /// first. Blocks only if the worker hasn't finished `index` yet — usually
    /// it has, having overlapped the page thread's earlier work. Only called for
    /// dispatched indices.
    fn take(&mut self, index: usize) -> Result<boa_engine::RawScript, String> {
        if let Some(result) = self.buffered.remove(&index) {
            return result;
        }
        loop {
            match self.rx.recv() {
                Ok((i, result)) if i == index => return result,
                Ok((i, result)) => {
                    self.buffered.insert(i, result);
                }
                // All workers exited without producing `index` — only possible
                // if every spawn failed; surface a per-script error, page lives.
                Err(_) => return Err(String::from("parse worker exited without a result")),
            }
        }
    }
}

/// Dispatch raw-parse jobs for every external classic script that has a fetched
/// body, returning a [`ParsePool`] to collect them — or `None` when there are
/// fewer than two such scripts (a single bundle is one parse unit, so a pool
/// would only add thread overhead; the loop then parses inline as before).
///
/// One parser id is allocated per script from the page `Context` up front
/// (tagged-template call-site identity must stay unique across separately parsed
/// scripts, exactly as the in-context parse path allocates them), so this
/// borrows `ctx` briefly before any worker starts.
fn dispatch_parallel_parse(
    scripts: &[(Option<String>, String, Option<String>, usize)],
    externals: &[(String, Option<Vec<u8>>)],
    cache_hits: &std::collections::HashSet<usize>,
    ctx: &mut Context,
) -> Option<ParsePool> {
    // Kill switch: `TRUST_NO_PARALLEL_PARSE` forces the sequential path (parse
    // inline on this thread). A safety valve for a concurrency feature touching
    // the engine, and the A/B toggle for measuring the win.
    if std::env::var_os("TRUST_NO_PARALLEL_PARSE").is_some() {
        return None;
    }
    let mut jobs: Vec<(usize, String, Vec<u8>, u32)> = Vec::new();
    for (i, (src, _inline, type_attr, _node)) in scripts.iter().enumerate() {
        if !is_classic(type_attr) {
            continue;
        }
        // A CDN cache hit is rehydrated, not compiled, so it needs no parse —
        // keep it out of the pool (parsing it would be pure waste).
        if cache_hits.contains(&i) {
            continue;
        }
        // Inline scripts stay on the page thread (small, and the body is already
        // in hand); only external bundles — where the parse cost lives — go to
        // the pool. A not-fetched / ad-blocked external has no body to parse.
        let Some(src) = src else { continue };
        let Some((_, Some(body))) = externals.iter().find(|(k, _)| k == src) else {
            continue;
        };
        let id = ctx.next_parser_identifier();
        jobs.push((i, src.clone(), body.clone(), id));
    }
    if jobs.len() < 2 {
        return None;
    }
    let dispatched: std::collections::HashSet<usize> = jobs.iter().map(|(i, ..)| *i).collect();
    let n_workers = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(2)
        .min(jobs.len())
        .min(PARSE_CONCURRENCY);
    let queue = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::from(jobs)));
    let (tx, rx) = std::sync::mpsc::channel();
    for w in 0..n_workers {
        let queue = std::sync::Arc::clone(&queue);
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("trust-parse-{w}"))
            // Same 64MB stack as the page thread: Boa's recursive-descent parser
            // can overflow a 2 MB stack on a big bundle (an uncatchable abort,
            // trap #1). The panic hook in main.rs covers `trust-*` threads.
            .stack_size(PAGE_STACK)
            .spawn(move || {
                loop {
                    let job = queue.lock().expect("parse queue poisoned").pop_front();
                    let Some((index, name, body, id)) = job else {
                        break;
                    };
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let src = Source::from_bytes(&body).with_path(std::path::Path::new(&name));
                        boa_engine::Script::raw_parse(src, id)
                    }))
                    .unwrap_or_else(|_| Err(String::from("raw parse panicked (Boa bug)")));
                    if tx.send((index, result)).is_err() {
                        break; // page thread is gone; stop early
                    }
                }
            });
        if spawned.is_err() {
            // A worker failed to spawn; the others still drain the queue. Its tx
            // clone drops with the failed closure, so the channel still closes
            // when the live workers finish.
        }
    }
    drop(tx); // the spawned workers hold the only live senders now
    Some(ParsePool {
        rx,
        buffered: std::collections::HashMap::new(),
        dispatched,
    })
}

/// The cross-page CDN compiled-code cache (JS-engine performance plan, Phase 2)
/// — the keystone `CodeBlockImage`'s second consumer, after the prelude cache.
///
/// External classic scripts (jQuery, D3, Vue, Lit, React … served from a CDN)
/// are byte-identical across the pages of a session, yet Boa re-parses and
/// re-compiles each one per page. This process-global cache keys a detached,
/// `Send + Sync` `CodeBlockImage` by the script's source hash so a given library
/// is parsed + compiled ONCE per process and merely *rehydrated* (re-allocated
/// into the page thread's GC heap) on every later page. RAM-only and
/// session-lifetime, the same ethos as the keep-alive `POOL` and the cookie jar.
///
/// Only **realm-portable** blocks are admitted (see the gate in
/// [`run_external_classic`]): a classic script runs in the page's global scope,
/// and a top-level `let`/`const`/`class` compiles to a binding referenced by
/// SLOT INDEX into the realm's shared, accumulating global declarative
/// environment — an index that depends on what ran before it on the page, so its
/// block is NOT valid on another page. Every real UMD/IIFE bundle (`(function(){
/// …})()` or `var Lib = (function(){…})()` at top level) is portable; anything
/// declaring a global lexical falls back cleanly to a normal per-page compile
/// and is marked [`CdnEntry::NotReusable`] so the gate isn't re-evaluated on
/// every visit. `TRUST_NO_CDN_CACHE` disables it (the A/B toggle + safety valve,
/// like `TRUST_NO_PRELUDE_CACHE`).
enum CdnEntry {
    /// A realm-portable compiled block, reusable on any page this session.
    Reusable(std::sync::Arc<boa_engine::vm::CodeBlockImage>),
    /// This source compiled to a non-portable block — don't cache or re-check it.
    NotReusable,
}

/// The result of a [`CDN_CACHE`] probe.
enum CdnLookup {
    Reusable(std::sync::Arc<boa_engine::vm::CodeBlockImage>),
    NotReusable,
    Absent,
}

/// Bounded image set (a RAM lid; a session loads a small, fixed set of CDN
/// libraries). On overflow the oldest insertion is evicted — a hostile-page
/// guard, not a working constraint.
const CDN_CACHE_MAX: usize = 64;

/// Discriminates cache entries by engine build: a different binary may compile
/// the same source to an incompatible image, so its hash space must not overlap.
/// The cache never persists across processes, so within one run this is
/// constant; folding it into the key is defensive and documents the keying rule
/// the plan calls for (source hash + engine build + flags).
const CDN_CACHE_BUILD_TAG: &str = concat!("trust-cdn-cache-v1/", env!("CARGO_PKG_VERSION"));

struct CdnCache {
    map: std::collections::HashMap<[u8; 32], CdnEntry>,
    /// Keys in insertion order, for FIFO eviction past `CDN_CACHE_MAX`.
    order: VecDeque<[u8; 32]>,
}

static CDN_CACHE: std::sync::OnceLock<std::sync::Mutex<CdnCache>> = std::sync::OnceLock::new();

fn cdn_cache() -> &'static std::sync::Mutex<CdnCache> {
    CDN_CACHE.get_or_init(|| {
        std::sync::Mutex::new(CdnCache {
            map: std::collections::HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

/// Whether the CDN cache is active. `TRUST_NO_CDN_CACHE` forces the cold compile
/// path for every external script.
fn cdn_cache_enabled() -> bool {
    std::env::var_os("TRUST_NO_CDN_CACHE").is_none()
}

/// Cache key for a script body: a strong (collision-free in practice) hash over
/// the engine-build tag, the length, and the bytes.
fn cdn_cache_key(body: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(CDN_CACHE_BUILD_TAG.as_bytes());
    hasher.update((body.len() as u64).to_le_bytes());
    hasher.update(body);
    hasher.finalize().into()
}

/// Probe the cache, cloning the (cheap, `Arc`) reusable image out under the lock.
/// A poisoned lock degrades to a miss (the cache simply stops helping).
fn cdn_cache_lookup(key: &[u8; 32]) -> CdnLookup {
    let Ok(cache) = cdn_cache().lock() else {
        return CdnLookup::Absent;
    };
    match cache.map.get(key) {
        Some(CdnEntry::Reusable(image)) => CdnLookup::Reusable(image.clone()),
        Some(CdnEntry::NotReusable) => CdnLookup::NotReusable,
        None => CdnLookup::Absent,
    }
}

/// Record a cache decision for `key` (the first writer wins a race; a poisoned
/// lock silently drops the entry). Evicts the oldest insertion past the lid.
fn cdn_cache_put(key: [u8; 32], entry: CdnEntry) {
    let Ok(mut cache) = cdn_cache().lock() else {
        return;
    };
    if cache.map.contains_key(&key) {
        return;
    }
    while cache.map.len() >= CDN_CACHE_MAX {
        match cache.order.pop_front() {
            Some(oldest) => {
                cache.map.remove(&oldest);
            }
            None => break,
        }
    }
    cache.order.push_back(key);
    cache.map.insert(key, entry);
}

/// Indices (into `scripts`) of external classic scripts whose compiled image is
/// already cached as reusable. They are rehydrated rather than compiled, so they
/// need no parse and are kept OUT of the parallel-parse pool (a worker parse of
/// a script we'll never compile would be pure waste).
fn cdn_cache_hits(
    scripts: &[(Option<String>, String, Option<String>, usize)],
    externals: &[(String, Option<Vec<u8>>)],
) -> std::collections::HashSet<usize> {
    let mut hits = std::collections::HashSet::new();
    if !cdn_cache_enabled() {
        return hits;
    }
    for (i, (src, _inline, type_attr, _node)) in scripts.iter().enumerate() {
        if !is_classic(type_attr) {
            continue;
        }
        let Some(src) = src else { continue };
        let Some((_, Some(body))) = externals.iter().find(|(k, _)| k == src) else {
            continue;
        };
        if matches!(
            cdn_cache_lookup(&cdn_cache_key(body)),
            CdnLookup::Reusable(_)
        ) {
            hits.insert(i);
        }
    }
    hits
}

/// Run one external classic script, going through the CDN compile cache.
///
/// - **Hit** (a reusable image is cached): rehydrate + run via the carrier,
///   skipping parse AND compile. (`prepared` is `None` here — a hit is excluded
///   from the parallel-parse pool.)
/// - **Miss** (`Absent`): compile this page's copy — from the worker `prepared`
///   raw parse when present, else parsed inline — then apply the
///   realm-portability gate and, if it passes, store the detached image for
///   later pages. The gate is two cheap fork checks:
///   [`Script::global_declarations_are_replayable`] (the script declares no
///   global *lexical* — `let`/`const`/`class` — nor Annex-B block-function
///   binding, so running its `<main>` recreates all its globals; top-level
///   `var`/`function` are fine, created by name from the bytecode) AND
///   [`CodeBlock::is_realm_portable`](boa_engine::vm::CodeBlock::is_realm_portable)
///   (it reads no prior script's global-declarative slot). Both must hold; a
///   real UMD/IIFE bundle passes trivially.
/// - **`NotReusable`** (a prior page proved it non-portable): compile + run, no
///   re-check, no store.
///
/// Same budget gate, `catch_unwind`, and error/panic accounting as
/// [`run_script`], so a bad script costs only itself.
fn run_external_classic(
    ctx: &mut Context,
    name: &str,
    body: &[u8],
    prepared: Option<Result<boa_engine::RawScript, String>>,
    budget: &Budget,
    outcome: &mut Outcome,
) {
    let key = cdn_cache_enabled().then(|| cdn_cache_key(body));
    let lookup = key.as_ref().map_or(CdnLookup::Absent, cdn_cache_lookup);
    if let CdnLookup::Reusable(image) = &lookup {
        phase(&format!(
            "cdn cache HIT {name} (rehydrate, no parse/compile)"
        ));
        run_rehydrated_image(ctx, name, image, budget, outcome);
        return;
    }
    // Store the freshly compiled image only on a true miss (`Absent`); a
    // `NotReusable` entry means we already decided this source can't be cached.
    let store_key = match lookup {
        CdnLookup::Absent => key,
        CdnLookup::NotReusable | CdnLookup::Reusable(_) => None,
    };

    if budget.exhausted() || outcome.elapsed >= COMPUTE_BUDGET {
        outcome
            .errors
            .push(format!("{name}: skipped, page JS budget exhausted"));
        return;
    }
    // A raw-parse error from the worker pool surfaces exactly like a
    // `Script::parse` error inside `run_script`.
    let prepared = match prepared {
        Some(Ok(raw)) => Some(raw),
        Some(Err(err)) => {
            outcome.errors.push(format!("{name}: {err}"));
            return;
        }
        None => None,
    };
    let started = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        // Obtain the compiled script: either compile the worker's raw parse
        // (parse was off-thread; `compile_raw` does scope-analysis + bytecode,
        // charged to Compile), or parse + compile inline.
        let script = match prepared {
            Some(raw) => {
                let t = phase_begin();
                let s = boa_engine::Script::compile_raw(raw, None, ctx)?;
                phase_end(Phase::Compile, t);
                s
            }
            None => {
                let src = Source::from_bytes(body).with_path(std::path::Path::new(name));
                let t = phase_begin();
                let s = boa_engine::Script::parse(src, None, ctx)?;
                phase_end(Phase::Parse, t);
                let t = phase_begin();
                s.codeblock(ctx)?;
                phase_end(Phase::Compile, t);
                s
            }
        };
        // Realm-portability gate: cache the detached image only when the script
        // neither creates nor reads a global-declarative binding (so its block
        // runs identically on any page). `codeblock` is memoized — no recompile.
        if let Some(key) = store_key {
            let cb = script.codeblock(ctx)?;
            let reusable = script.global_declarations_are_replayable() && cb.is_realm_portable();
            phase(&format!(
                "cdn cache store {name} ({})",
                if reusable { "reusable" } else { "not reusable" }
            ));
            let entry = if reusable {
                CdnEntry::Reusable(std::sync::Arc::new(cb.to_image()))
            } else {
                CdnEntry::NotReusable
            };
            cdn_cache_put(key, entry);
        }
        let t = phase_begin();
        let r = script.evaluate(ctx);
        phase_end(Phase::Execute, t);
        r
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

/// Set `document.currentScript` (via the `__trust` bridge, like `readyState`)
/// to the node id of the classic script about to run — `None` clears it back
/// to null. A bare property write can't panic the VM, so it skips the
/// `run_script` budget/error machinery.
fn set_current_script(ctx: &mut Context, id: Option<usize>) {
    let src = match id {
        Some(id) => format!("__trust.currentScript={id};"),
        None => "__trust.currentScript=null;".to_string(),
    };
    let _ = ctx.eval(Source::from_bytes(src.as_bytes()));
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
    /// True once the page is past load, handling interactive dispatches. A
    /// fetch fired during a dispatch extends the (tight, compute-sized)
    /// dispatch deadline so the wire can finish — a click that asks an LLM for
    /// a reply must wait for the reply, not be cancelled at the 1s compute cap.
    /// Load fetches don't extend (the load already runs on `WALL_BUDGET`).
    dispatch: std::cell::Cell<bool>,
    /// The shared subresource cache, so the page's own `fetch()` can join
    /// an in-flight/done request for a chunk we already have.
    cache: std::sync::Arc<crate::http::PageCache>,
}

impl boa_engine::gc::Finalize for PageNet {}
// SAFETY: holds no GC-managed objects.
unsafe impl boa_engine::gc::Trace for PageNet {
    boa_engine::gc::empty_trace!();
}

/// Page-owned WebSockets (see `ws.rs`) — the transport socket.io rides. Each
/// `new WebSocket()` spawns a connection task; this maps the JS-visible id to
/// that task's outbound sender, and hands every task a clone of `events` so
/// inbound frames/open/close reach the actor (forwarded as `PageCmd::Ws`,
/// dispatched like a click). RAM-only, dies with the page, which drops the
/// senders → the tasks close their sockets and exit.
#[derive(boa_engine::JsData)]
struct PageWs {
    handle: tokio::runtime::Handle,
    page: url::Url,
    events: tokio::sync::mpsc::Sender<(usize, crate::ws::WsIn)>,
    sockets: RefCell<std::collections::HashMap<usize, tokio::sync::mpsc::Sender<crate::ws::WsOut>>>,
    next_id: std::cell::Cell<usize>,
}

impl boa_engine::gc::Finalize for PageWs {}
// SAFETY: holds no GC-managed objects (channels/handle/url/RefCell map).
unsafe impl boa_engine::gc::Trace for PageWs {
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

/// The geometry box map's cache: the DOM epoch it was built for, and each
/// element's pixel box keyed by node. Stale (epoch mismatch) entries are
/// rebuilt lazily on the next `__dom_rect` read — free for pages that never
/// measure, one layout pass for those that do, reused until the next mutation.
type GeomCache = (
    u64,
    std::collections::HashMap<crate::dom::NodeId, crate::layout::PxRect>,
);

/// Backing for the JS geometry APIs (`getBoundingClientRect`, `offset*`/
/// `client*`, IntersectionObserver/ResizeObserver). Holds the immutable layout
/// inputs the measure pass needs (the page base, content width in cells, the
/// terminal's cell pixel size, and whether borders draw) plus the lazily built,
/// epoch-keyed box map. See `sys_rect` and `layout::measure_boxes`.
#[derive(boa_engine::JsData)]
struct PageGeom {
    base: url::Url,
    width_cells: u16,
    cell_px: (u16, u16),
    borders: bool,
    cache: Rc<RefCell<GeomCache>>,
}

impl boa_engine::gc::Finalize for PageGeom {}
// SAFETY: holds no GC-managed objects.
unsafe impl boa_engine::gc::Trace for PageGeom {
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

/// Diagnostic (`TRUST_JS_PROFILE`): dump and reset the Boa VM sampling
/// profile — the hottest JS leaf frames since the last dump, each as
/// `source-url :: function`. The profiler samples the synchronous and async
/// instruction loops; it's zero-cost when the env var is unset. Invaluable for
/// engine-perf dives because hardware perf counters are locked on this host
/// (see CLAUDE.md) — and it disambiguates "stuck in JS" from "stuck in Rust"
/// (a phase with elapsed wall time but no samples is Rust-side, e.g. layout or
/// the cascade, not the engine).
fn dump_vm_profile(tag: &str) {
    let (total, prof) = boa_engine::vm::profile::take_vm_profile();
    if total == 0 {
        return;
    }
    eprintln!(
        "js : @{:>6}ms VM profile [{tag}] ({total} samples, hottest leaf frames)",
        crate::http::trace_ms()
    );
    for (k, n) in prof.iter().take(25) {
        eprintln!("{n:>10} ({:>4.1}%)  {k}", 100.0 * *n as f64 / total as f64);
    }
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
        ("__dom_contains", 2, sys_contains),
        ("__dom_children", 1, sys_children),
        ("__dom_next", 1, sys_next),
        ("__dom_prev", 1, sys_prev),
        ("__dom_node_type", 1, sys_node_type),
        ("__dom_tag", 1, sys_tag),
        ("__dom_get_attr", 2, sys_get_attr),
        ("__dom_computed", 2, sys_computed_style),
        ("__dom_rect", 1, sys_rect),
        ("__dom_set_attr", 3, sys_set_attr),
        ("__dom_remove_attr", 2, sys_remove_attr),
        ("__dom_attr_names", 1, sys_attr_names),
        ("__dom_text", 1, sys_text),
        ("__dom_set_text", 2, sys_set_text),
        ("__dom_inner_html", 1, sys_inner_html),
        ("__dom_set_inner_html", 2, sys_set_inner_html),
        ("__dom_load_frame", 3, sys_load_frame),
        ("__dom_outer_html", 1, sys_outer_html),
        ("__dom_insert_adjacent", 3, sys_insert_adjacent),
        ("__dom_query", 3, sys_query),
        ("__dom_matches", 2, sys_matches),
        ("__dom_get_by_id", 1, sys_get_by_id),
        ("__dom_upgrade_candidates", 2, sys_upgrade_candidates),
        ("__dom_ce_candidates", 1, sys_ce_candidates),
        ("__dom_clone", 2, sys_clone),
        ("__dom_doc_element", 0, sys_doc_element),
        ("__url_parse", 2, sys_url_parse),
        ("__dom_attach_shadow", 1, sys_attach_shadow),
        ("__dom_shadow_root", 1, sys_shadow_root),
        ("__dom_adopt_styles", 2, sys_adopt_styles),
        ("__css_parse", 1, sys_css_parse),
        ("__css_supports_selector", 1, sys_css_supports_selector),
        ("__dom_template_content", 1, sys_template_content),
        ("__http_fetch", 5, sys_http_fetch),
        ("__http_fetch_async", 5, sys_http_fetch_async),
        ("__dom_run_injected_script", 1, sys_run_injected_script),
        ("__cookie_get", 0, sys_cookie_get),
        ("__cookie_set", 1, sys_cookie_set),
        ("__storage_get", 2, sys_storage_get),
        ("__storage_set", 3, sys_storage_set),
        ("__storage_remove", 2, sys_storage_remove),
        ("__storage_clear", 1, sys_storage_clear),
        ("__storage_key", 2, sys_storage_key),
        ("__storage_len", 1, sys_storage_len),
        ("__ws_open", 2, sys_ws_open),
        ("__ws_send", 3, sys_ws_send),
        ("__ws_close", 3, sys_ws_close),
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

/// `__dom_contains(a, b)` → is `a` a STRICT ancestor of `b`? The subtree
/// match for MutationObserver: walking `b`'s parent chain here (one Rust
/// pointer walk) replaces a JS `parentNode` loop that would syscall+wrap per
/// hop — the trap #9 lesson (a body-rooted `subtree:true` observer tests this
/// on every mutation). Does not cross shadow boundaries (parent is null at a
/// shadow root), matching the default non-composed observation scope.
fn sys_contains(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let d = dom.borrow();
    let found = match (arg_node(&d, args, 0), arg_node(&d, args, 1)) {
        (Some(anc), Some(node)) => {
            let mut cur = d.node(node).parent;
            loop {
                match cur {
                    Some(p) if p == anc => break true,
                    Some(p) => cur = d.node(p).parent,
                    None => break false,
                }
            }
        }
        _ => false,
    };
    Ok(JsValue::from(found))
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

/// Geometry backing for `getBoundingClientRect`/`offset*`/`client*` and the
/// observers: the element's box as `[left, top, width, height]` in CSS pixels,
/// or null when it has no laid-out box (the prelude then falls back to the
/// viewport box). The map is built once per DOM epoch by one layout pass over
/// the live arena (`layout::measure_boxes`) and reused until the next mutation
/// — lazy, so a page that never measures pays nothing.
fn sys_rect(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some((base, width_cells, cell_px, borders, cache)) = ({
        let host = ctx.realm().host_defined();
        host.get::<PageGeom>().map(|g| {
            (
                g.base.clone(),
                g.width_cells,
                g.cell_px,
                g.borders,
                g.cache.clone(),
            )
        })
    }) else {
        return Ok(JsValue::null());
    };
    let dom = page_dom(ctx);
    let d = dom.borrow();
    let Some(id) = arg_node(&d, args, 0) else {
        return Ok(JsValue::null());
    };
    let epoch = d.epoch();
    {
        let mut c = cache.borrow_mut();
        if c.0 != epoch {
            let (forms, controls) = crate::http::extract_forms_arena(&d, &base, None);
            c.1 = crate::layout::measure_boxes(
                &d,
                &base,
                width_cells as usize,
                &forms,
                &controls,
                cell_px,
                borders,
            );
            c.0 = epoch;
        }
    }
    let rect = cache.borrow().1.get(&id).copied();
    Ok(match rect {
        Some(r) => JsArray::from_iter(
            [
                JsValue::from(r.left),
                JsValue::from(r.top),
                JsValue::from(r.width),
                JsValue::from(r.height),
            ],
            ctx,
        )
        .into(),
        None => JsValue::null(),
    })
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

/// `__dom_load_frame(frameId, html, baseUrl)` — install `html` as the
/// iframe's nested document (HTML "process the iframe attributes" / "navigate
/// an iframe"). The prelude calls this for both `src` (after fetching) and
/// `srcdoc`; the heavy lifting (full document parse, replace the content
/// navigable, absolutize the frame's relative URLs) is `Dom::install_frame_document`.
fn sys_load_frame(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let html = arg_str(args, 1, ctx);
    let base = arg_str(args, 2, ctx);
    let dom = page_dom(ctx);
    let mut d = dom.borrow_mut();
    if let Some(frame) = arg_node(&d, args, 0) {
        d.install_frame_document(frame, &html, &base);
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

/// `(rootId, name)` → the composed-tree element ids (shadow-piercing, document
/// order) whose tag is `name`. Backs `customElements.define`'s catch-up upgrade
/// without the per-node JS tree walk it used to do.
fn sys_upgrade_candidates(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_str(args, 1, ctx).to_ascii_lowercase();
    let dom = page_dom(ctx);
    let ids = {
        let d = dom.borrow();
        match arg_node(&d, args, 0) {
            Some(root) => d.elements_by_tag_composed(root, &name),
            None => Vec::new(),
        }
    };
    Ok(ids_array(ids, ctx))
}

/// `(rootId)` → the composed-subtree element ids (root included, shadow-
/// piercing) whose tag is a custom-element name (contains a hyphen). Backs
/// `ceScan`'s insertion-time upgrade/connect pass without the per-node JS walk.
fn sys_ce_candidates(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dom = page_dom(ctx);
    let ids = {
        let d = dom.borrow();
        match arg_node(&d, args, 0) {
            Some(root) => d.custom_elements_composed(root),
            None => Vec::new(),
        }
    };
    Ok(ids_array(ids, ctx))
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
    headers: Vec<(String, String)>,
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
    // A fetch fired during an interactive dispatch extends the tight (1s,
    // compute-sized) dispatch deadline up to the network-inclusive wall budget,
    // so a slow wire — an LLM streaming a reply — can finish instead of being
    // cancelled. The job-drainer re-reads the deadline, so this takes effect
    // mid-drain. Load fetches don't extend (the load already runs on the wall
    // budget), so this never loosens a load.
    if net.dispatch.get() {
        net.budget.extend_at_least(DISPATCH_NET_GRACE);
    }
    let mut request = crate::http::Request {
        method,
        url: resolved,
        body,
        headers,
    };
    // A document-initiated request carries the page's Referer (browser
    // default policy) unless the page set one itself.
    crate::http::set_referrer(&mut request, &net.page);
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
    headers: Vec<(String, String)>,
) -> Option<crate::http::Response> {
    let (handle, request) = page_net_prepare(ctx, target, method, body, headers)?;
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
    let (handle, request) = page_net_prepare(
        ctx,
        resolved.as_str(),
        String::from("GET"),
        None,
        Vec::new(),
    )?;
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
    let (handle, request) = page_net_prepare(
        ctx,
        resolved.as_str(),
        String::from("GET"),
        None,
        Vec::new(),
    )?;
    Some(cache.fetch(&handle, request.url))
}

/// `__http_fetch(url, method, body|null, content_type|null)` →
/// `[status, content_type, body_text]` or null (blocked/failed/no net).
/// Synchronous from the page's view; the request runs on the tokio
/// runtime while the JS thread blocks. The one blocking caller is legacy
/// synchronous XHR — async fetch/XHR go through `sys_http_fetch_async`.
fn sys_http_fetch(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (url_arg, method, body, headers) = fetch_args(args, ctx);
    Ok(match page_net_fetch(ctx, &url_arg, method, body, headers) {
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
    let (url_arg, method, body, headers) = fetch_args(args, ctx);
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
    match page_net_prepare(ctx, &url_arg, method, body, headers) {
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

/// `__ws_open(url, protocols)` → a socket id (>0), or -1 if WebSockets aren't
/// available here / the URL is bad / the target is blocked. Spawns the
/// connection task (`ws::connect`); inbound frames + open/close arrive as
/// `PageCmd::Ws`. The page's bundled socket.io-client runs the protocol on top.
fn sys_ws_open(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let url_arg = match args.first() {
        Some(v) => v.to_string(ctx)?.to_std_string_lossy(),
        None => return Ok(JsValue::new(-1)),
    };
    let host = ctx.realm().host_defined();
    let Some(wsh) = host.get::<PageWs>() else {
        return Ok(JsValue::new(-1)); // no-net page / one-shot transform
    };
    let Ok(resolved) = wsh.page.join(&url_arg) else {
        return Ok(JsValue::new(-1));
    };
    if !matches!(resolved.scheme(), "ws" | "wss") {
        return Ok(JsValue::new(-1));
    }
    // Same private-address-pivot guard as fetch, with ws(s) mapped to http(s).
    let mut http_equiv = resolved.clone();
    let _ = http_equiv.set_scheme(if resolved.scheme() == "wss" {
        "https"
    } else {
        "http"
    });
    if !crate::http::subresource_allowed(&wsh.page, &http_equiv) {
        return Ok(JsValue::new(-1));
    }
    let id = wsh.next_id.get();
    wsh.next_id.set(id + 1);
    let origin = wsh.page.origin().ascii_serialization();
    let cookie = {
        let c = crate::http::cookies_for_request(&http_equiv);
        (!c.is_empty()).then_some(c)
    };
    let out_tx = crate::ws::connect(
        resolved,
        origin,
        cookie,
        &wsh.handle,
        id,
        wsh.events.clone(),
    );
    wsh.sockets.borrow_mut().insert(id, out_tx);
    Ok(JsValue::new(id as i32))
}

/// `__ws_send(id, data, isBinary)` — queue a message on a socket. `data` is a
/// string: UTF-8 text, or (isBinary) a latin1 byte string (each char a byte).
fn sys_ws_send(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = args.first().map_or(Ok(-1.0), |v| v.to_number(ctx))? as i64;
    let data = match args.get(1) {
        Some(v) => v.to_string(ctx)?.to_std_string_lossy(),
        None => String::new(),
    };
    let is_binary = args.get(2).is_some_and(JsValue::to_boolean);
    let host = ctx.realm().host_defined();
    let Some(wsh) = host.get::<PageWs>() else {
        return Ok(JsValue::new(false));
    };
    if id < 0 {
        return Ok(JsValue::new(false));
    }
    if let Some(tx) = wsh.sockets.borrow().get(&(id as usize)) {
        let msg = if is_binary {
            crate::ws::WsOut::Binary(data.chars().map(|c| c as u8).collect())
        } else {
            crate::ws::WsOut::Text(data)
        };
        // Non-blocking: the channel is generous; chat sends are low-rate.
        let _ = tx.try_send(msg);
        return Ok(JsValue::new(true));
    }
    Ok(JsValue::new(false))
}

/// `__ws_close(id, code, reason)` — close a socket.
fn sys_ws_close(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = args.first().map_or(Ok(-1.0), |v| v.to_number(ctx))? as i64;
    let code = args.get(1).map_or(Ok(1000.0), |v| v.to_number(ctx))? as u16;
    let reason = match args.get(2) {
        Some(v) if !v.is_null_or_undefined() => v.to_string(ctx)?.to_std_string_lossy(),
        _ => String::new(),
    };
    let host = ctx.realm().host_defined();
    let Some(wsh) = host.get::<PageWs>() else {
        return Ok(JsValue::undefined());
    };
    if id >= 0
        && let Some(tx) = wsh.sockets.borrow().get(&(id as usize))
    {
        let _ = tx.try_send(crate::ws::WsOut::Close(code, reason));
    }
    Ok(JsValue::undefined())
}

/// `__dom_run_injected_script(nodeId)` — run a `<script>` element that page
/// JS inserted into the live document (the universal SDK-loader idiom
/// `document.body.appendChild(scriptEl)`: reCAPTCHA/hCaptcha, lazy analytics,
/// payment/embed widgets, A/B tools). A real browser fetches+executes such a
/// script; TRust never did, so a runtime-injected dependency silently never
/// loaded — pixiv's login injects `recaptcha/enterprise.js` then polls
/// `window.grecaptcha` forever, so the submit hung. Classic scripts only
/// (the prelude already gates type + first-insertion): a `src` is fetched
/// then evaluated, inline text is evaluated, and a `load`/`error` event fires
/// on the element for code that waits on `script.onload`. The fetch reuses
/// the same cap-/`subresource_allowed`-gated async job `fetch()` uses, so it
/// overlaps with other work and can't pivot to private address space.
fn sys_run_injected_script(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (node_id, src, text) = {
        let dom = page_dom(ctx);
        let d = dom.borrow();
        let Some(id) = arg_node(&d, args, 0) else {
            return Ok(JsValue::undefined());
        };
        (
            id,
            d.attr(id, "src").map(str::to_string),
            d.text_content(id),
        )
    };
    match src {
        Some(src) if !src.trim().is_empty() => {
            match page_net_prepare(ctx, &src, String::from("GET"), None, vec![]) {
                // Blocked/capped/cross-private: report a failed load like a browser.
                None => fire_script_event(ctx, node_id, "error"),
                Some((_handle, request)) => {
                    phase(&format!("src: INJECT {}", request.url));
                    let name = request.url.to_string();
                    let realm = ctx.realm().clone();
                    let job = NativeAsyncJob::with_realm(
                        async move |cell: &RefCell<&mut Context>| {
                            // Await without borrowing the context (so injected
                            // loads overlap like every other async fetch).
                            let result = crate::http::fetch(&request).await;
                            let mut guard = cell.borrow_mut();
                            match result {
                                Ok(resp) => {
                                    let body =
                                        crate::http::decode_body(&resp.content_type, &resp.body);
                                    eval_injected(&mut guard, &name, body.as_bytes());
                                    fire_script_event(&mut guard, node_id, "load");
                                }
                                Err(_) => fire_script_event(&mut guard, node_id, "error"),
                            }
                            Ok(JsValue::undefined())
                        },
                        realm,
                    );
                    ctx.enqueue_job(job.into());
                }
            }
        }
        _ if !text.trim().is_empty() => {
            // Inline injected script: evaluate its text. Run as a (ready) async
            // job so we don't re-enter `ctx.eval` from inside this syscall.
            let realm = ctx.realm().clone();
            let job = NativeAsyncJob::with_realm(
                async move |cell: &RefCell<&mut Context>| {
                    let mut guard = cell.borrow_mut();
                    eval_injected(&mut guard, "injected-inline", text.as_bytes());
                    Ok(JsValue::undefined())
                },
                realm,
            );
            ctx.enqueue_job(job.into());
        }
        _ => {}
    }
    Ok(JsValue::undefined())
}

/// Evaluate an injected script's source in the page realm, routing any error
/// to `__trust.errors` (so the `JS:n!` badge + diagnostics see it) and
/// surviving a Boa VM panic — one bad injected script can't kill the actor.
fn eval_injected(ctx: &mut Context, name: &str, source: &[u8]) {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Same exact `ctx.eval` unroll as `run_script`, so a dynamically
        // injected `<script>` (webpack chunk loaders inject many, sometimes
        // large, ones) lands in the parse/compile/execute split instead of
        // being invisible — it runs inside an async job future the executor
        // awaits, which the per-loop drain timer doesn't cover.
        let src = Source::from_bytes(source).with_path(std::path::Path::new(name));
        let t = phase_begin();
        let script = boa_engine::Script::parse(src, None, ctx)?;
        phase_end(Phase::Parse, t);
        let t = phase_begin();
        script.codeblock(ctx)?;
        phase_end(Phase::Compile, t);
        let t = phase_begin();
        let r = script.evaluate(ctx);
        phase_end(Phase::Execute, t);
        r
    }));
    if let Ok(Err(err)) = res {
        let msg = format!("{name}: {err}");
        let esc = msg
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        let _ = ctx.eval(Source::from_bytes(
            format!("__trust.errors.push(\"{esc}\")").as_bytes(),
        ));
    }
}

/// Fire a `load`/`error` event on an injected script element (and call its
/// `on<type>` handler), for loaders that wait on `script.onload`.
fn fire_script_event(ctx: &mut Context, node_id: usize, ty: &str) {
    let _ = ctx.eval(Source::from_bytes(
        format!("__trust.scriptEvent({node_id}, \"{ty}\")").as_bytes(),
    ));
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

/// Parse the shared `(url, method, body, content_type, headers)` arguments of
/// the fetch syscalls: method normalized, body paired with its content type,
/// extra request headers parsed from the newline-delimited `k\nv\nk\nv` blob
/// the prelude builds from `setRequestHeader`/`init.headers`.
#[allow(clippy::type_complexity)]
fn fetch_args(
    args: &[JsValue],
    ctx: &mut Context,
) -> (
    String,
    String,
    Option<(String, Vec<u8>)>,
    Vec<(String, String)>,
) {
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
    let headers = args
        .get(4)
        .filter(|v| !v.is_null_or_undefined())
        .map(|_| parse_header_blob(&arg_str(args, 4, ctx)))
        .unwrap_or_default();
    (url_arg, method, body, headers)
}

/// Parse a `name\nvalue\nname\nvalue` header blob into pairs (empty names
/// dropped). The prelude joins headers this way; values hold no newlines.
fn parse_header_blob(blob: &str) -> Vec<(String, String)> {
    if blob.is_empty() {
        return Vec::new();
    }
    blob.split('\n')
        .collect::<Vec<_>>()
        .chunks(2)
        .filter_map(|c| match c {
            [k, v] if !k.is_empty() => Some(((*k).to_string(), (*v).to_string())),
            _ => None,
        })
        .collect()
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
            let t = phase_begin();
            let m = boa_engine::Module::parse(
                Source::from_bytes(&cached.body).with_path(std::path::Path::new(&key)),
                None,
                &mut ctx,
            )?;
            phase_end(Phase::Parse, t);
            m
        };
        self.modules.borrow_mut().insert(key, module.clone());
        Ok(module)
    }

    /// Populate `import.meta` for a module. We expose `import.meta.url` — the
    /// module's own absolute URL (the path we attach at parse time) — which
    /// bundlers rely on to resolve sibling chunks: Vite/SvelteKit's preload
    /// helper does `new URL("../nodes/x.js", import.meta.url)`, so a missing
    /// `url` makes every relative resolution throw "Invalid URL" and the app
    /// never boots.
    fn init_import_meta(
        self: Rc<Self>,
        import_meta: &boa_engine::JsObject,
        module: &boa_engine::Module,
        context: &mut Context,
    ) {
        let url = module
            .path()
            .and_then(|p| p.to_str())
            .map(str::to_string)
            .or_else(|| self.page.as_ref().map(url::Url::to_string))
            .unwrap_or_default();
        let _ = import_meta.set(
            boa_engine::js_string!("url"),
            JsString::from(url),
            false,
            context,
        );
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
        let t = phase_begin();
        let m = boa_engine::Module::parse(
            Source::from_bytes(source).with_path(std::path::Path::new(path)),
            None,
            ctx,
        );
        phase_end(Phase::Parse, t);
        m
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
fn load_page(
    html: &str,
    env: &PageEnv,
    ws_events: Option<tokio::sync::mpsc::Sender<(usize, crate::ws::WsIn)>>,
) -> Result<LoadedPage, Outcome> {
    phase("load_page start (DOM parse)");
    phases_reset();
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
                dispatch: std::cell::Cell::new(false),
                cache: env.cache.clone(),
            });
        }
        // Register the WebSocket host BEFORE any script runs. socket.io (Open
        // WebUI and friends) opens its WS during page load / DOMContentLoaded,
        // so the very first `new WebSocket(...)` must already find a `PageWs`,
        // or `__ws_open` returns -1 and the socket is never opened — the page
        // then has no transport for its streamed reply, and socket.io's
        // reconnect timer is frozen at rest so it never retries. Only the
        // resident-actor path supplies a `ws_events` sender; the one-shot
        // `transform` gets None (no actor to forward inbound frames to).
        if let (Some(ws_tx), Some(handle), Some(page)) =
            (ws_events, env.net.clone(), parsed_url.clone())
        {
            host.insert(PageWs {
                handle,
                page,
                events: ws_tx,
                sockets: RefCell::new(std::collections::HashMap::new()),
                next_id: std::cell::Cell::new(1),
            });
        }
        // Geometry backing: a layout pass over this same arena answers the JS
        // box APIs. Needs an absolute base to resolve hrefs during layout; if
        // the page URL didn't parse, `__dom_rect` is simply absent and the
        // getters keep their viewport-box fallback.
        if let Some(base) = parsed_url.clone() {
            host.insert(PageGeom {
                base,
                width_cells: viewport.0,
                cell_px,
                borders: crate::layout::borders_enabled(),
                cache: Rc::new(RefCell::new((u64::MAX, std::collections::HashMap::new()))),
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
    run_prelude(&mut ctx, &budget, &mut outcome);
    if !outcome.errors.is_empty() {
        // The prelude is ours: if it broke, render without JS and say so.
        return Err(outcome);
    }
    phase(&format!(
        "prelude done; {} top-level scripts",
        scripts.len()
    ));

    // CDN compile cache (Phase 2): an external classic library compiled on an
    // earlier page of this session is already a detached image — these scripts
    // will be rehydrated, not compiled, so they're excluded from the parse pool
    // below and rehydrated in the loop.
    let cache_hits = cdn_cache_hits(&scripts, externals);

    // Parallel parse (Step 5a): external classic scripts raw-parse on a worker
    // pool NOW, overlapping the rest of this thread's work (the prelude already
    // ran; earlier scripts' execution and later scripts' parse now overlap). The
    // loop below still compiles + runs each in document order — only the lex/
    // parse phase moved off this thread. Cache hits are skipped (no parse needed).
    let mut parse_pool = dispatch_parallel_parse(&scripts, externals, &cache_hits, &mut ctx);

    for (i, (src, inline, type_attr, node)) in scripts.iter().enumerate() {
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
        // `document.currentScript` is the classic script element while its
        // own code runs (null for modules and between scripts) — SvelteKit's
        // bootstrap reads `document.currentScript.parentElement` to find its
        // mount node, so without this the whole app fails to start.
        set_current_script(&mut ctx, Some(*node));
        match src {
            Some(src) => match externals.iter().find(|(k, _)| k == src) {
                Some((_, Some(body))) => {
                    // Route through the CDN cache: a reusable image is rehydrated
                    // (no parse, no compile); otherwise compile this page's copy —
                    // from the worker pool's raw parse when it took this script,
                    // else inline — and cache the image if it's realm-portable.
                    let prepared = parse_pool
                        .as_mut()
                        .filter(|p| p.was_dispatched(i))
                        .map(|pool| pool.take(i));
                    run_external_classic(&mut ctx, src, body, prepared, &budget, &mut outcome);
                }
                _ => {
                    // A deliberately blocked ad/tracker script is expected,
                    // not a page error — don't light the JS-error badge for it.
                    let blocked = parsed_url
                        .as_ref()
                        .and_then(|b| b.join(src).ok())
                        .and_then(|u| u.host_str().map(crate::http::is_ad_or_tracker_host))
                        .unwrap_or(false);
                    if !blocked {
                        outcome.errors.push(format!("{src}: not fetched"));
                    }
                }
            },
            None => {
                let name = format!("inline#{}", i + 1);
                run_script(&mut ctx, &name, inline.as_bytes(), &budget, &mut outcome);
            }
        }
        if outcome.panicked {
            // Engine bug: what the page built so far still renders. (Don't
            // touch the VM further — currentScript is moot once we break.)
            break;
        }
        set_current_script(&mut ctx, None);
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
        dump_vm_profile("scripts");
        run_script(
            &mut ctx,
            "DOMContentLoaded",
            b"__trust.readyState = \"interactive\"; __trust.hydrateFrames(); __trust.fire(document, \"DOMContentLoaded\", true);",
            &budget,
            &mut outcome,
        );
    }

    drain_js_side(&mut ctx, &mut outcome);
    drain_rejections(&hooks, &mut outcome);
    dump_vm_profile("DOMContentLoaded");
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
        // Fire `load` on any image whose reveal a handler is waiting for, so a
        // fade-in-on-load image is shown rather than hidden at `opacity:0`.
        settle_image_loads(&mut page.ctx, &page.budget, MAX_TICKS, &mut page.outcome);
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
    // Which JS functions burned the settle phase (under TRUST_JS_PROFILE) — the
    // settle is where a data-driven SPA spends its seconds, and the load_page
    // dumps only cover up to DOMContentLoaded, so without this the dominant
    // phase is unsampled. Pairs with the phase split below.
    dump_vm_profile("settle");
    // Step 1 decision-gate output: the whole-load parse/compile/execute split
    // (under TRUST_JS_PHASE). `page.started` was stamped at load start, so this
    // is the full load+settle wall — including this page's settle execution,
    // which `engine_profile` (one bundle in isolation) can't see.
    report_phases(page.started.elapsed());
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
        // `__trust.tick` advances virtual time and FIRES due timer callbacks
        // (page JS) synchronously, so its execution is real engine work — time
        // it into the execute bucket. It runs via a direct `ctx.eval` (not
        // `run_script`/a job), so without this it would be invisible to the
        // profiler and mis-attributed to "Rust-side" in the gate split.
        let t = phase_begin();
        let ticked = ctx.eval(Source::from_bytes(b"__trust.tick(1000)"));
        phase_end(Phase::Execute, t);
        match ticked {
            Ok(v) if v.to_boolean() => ticks += 1,
            _ => {
                phase(&format!("settle: {ticks} ticks, quiescent"));
                break;
            }
        }
    }
}

/// How many scan→settle passes the synthetic-image-load driver makes. One
/// reveals the current content; a couple more catch images a `load` handler
/// inserts in turn (lightGallery loads the slide, then preloads its
/// neighbours). Bounded so a pathological load handler can't loop forever.
const IMG_LOAD_PASSES: usize = 3;

/// Schedule synthetic `<img>` `load` events for anything waiting on one, then
/// settle so the handlers run, repeating a few passes to catch images those
/// handlers insert (see `trust.scanImageLoads`). The reveal-on-load idiom would
/// otherwise leave such images at `opacity:0` forever (a headless DOM never
/// fires `load`), and the layout drops an invisible image. A no-op on a page
/// where nothing listens for `load`.
fn settle_image_loads(ctx: &mut Context, budget: &Budget, max_ticks: usize, outcome: &mut Outcome) {
    for _ in 0..IMG_LOAD_PASSES {
        let scheduled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let t = phase_begin();
            let n = ctx
                .eval(Source::from_bytes(b"__trust.scanImageLoads()"))
                .ok()
                .and_then(|v| v.as_number())
                .unwrap_or(0.0);
            phase_end(Phase::Execute, t);
            n
        }))
        .unwrap_or(0.0);
        if scheduled < 1.0 {
            break;
        }
        settle(ctx, budget, max_ticks, outcome);
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
                let cell = RefCell::new(ctx);
                handle.block_on(async {
                    let fut = exec.run_jobs_async(&cell);
                    tokio::pin!(fut);
                    // Drive the job loop until it finishes (all promises + fetches
                    // settled) or the budget deadline passes. The deadline is
                    // RE-READ each slice because a dispatch-time fetch pushes it
                    // out (wire time): a stale one-shot timeout would cancel an
                    // in-flight LLM completion at the 1s compute cap. A slice
                    // timing out doesn't drop `fut`, so the in-flight request
                    // survives to the next slice; only the final deadline drops it.
                    loop {
                        let remaining = budget.remaining();
                        if remaining.is_zero() {
                            break Ok(()); // deadline reached: render what we have
                        }
                        let slice = remaining.min(Duration::from_millis(200));
                        match tokio::time::timeout(slice, &mut fut).await {
                            Ok(r) => break r,   // job loop finished
                            Err(_) => continue, // re-read the (maybe extended) deadline
                        }
                    }
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
/// Apply ONLY the CSS cascade to a page — no JS. Parses the HTML, attaches
/// the (already-fetched) external stylesheets, and serializes; the serializer
/// bakes every cascaded property onto each element, exactly as the full JS
/// pipeline does. Used whenever JS won't run — a page with no `<script>`,
/// `set js off`, or the JS-load-timeout fallback — so the page still lays out
/// per its OWN stylesheets (flex, grid, visibility:hidden, …) instead of bare
/// UA defaults. Without this, e.g. GitHub's code view (when its heavy JS times
/// out) renders line numbers stacked above the code and nav menus expanded,
/// because `display:flex`/`visibility:hidden` from its sheets never apply.
pub fn css_bake(
    html: &str,
    sheets: &[(String, String)],
    viewport: (u16, u16),
    cell_px: (u16, u16),
) -> String {
    let dom = css_prepare(html, viewport, cell_px);
    css_finish(dom, sheets)
}

/// Parse a page into an arena with its viewport set — the front half of the
/// no-JS `css_bake`, split out so `http::css_only` can load the page's frames
/// (an async fetch + `Dom::install_frame_document`) between parse and
/// serialize, the script-less mirror of the JS pipeline's frame loading.
pub fn css_prepare(html: &str, viewport: (u16, u16), cell_px: (u16, u16)) -> Dom {
    let mut dom = Dom::parse_document(html);
    dom.set_viewport_px(
        u32::from(viewport.0) * u32::from(cell_px.0.max(1)),
        u32::from(viewport.1) * u32::from(cell_px.1.max(1)),
    );
    dom
}

/// Attach the page's external sheets and serialize the cascade-baked DOM —
/// the back half of `css_bake`.
pub fn css_finish(mut dom: Dom, sheets: &[(String, String)]) -> String {
    if !sheets.is_empty() {
        dom.attach_external_sheets(sheets);
    }
    dom.serialize(DOCUMENT)
}

pub fn transform(html: &str, env: &PageEnv) -> (String, Outcome) {
    match load_page(html, env, None) {
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
    /// A WebSocket event from a page-opened socket (the WS task forwards inbound
    /// frames + open/close here). Dispatched like a click: fire the JS event,
    /// settle, re-render. This is how a socket.io stream (chat tokens) drives
    /// the page with no busy-poll — a frame is just another event.
    Ws { id: usize, event: crate::ws::WsIn },
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
    /// A CLICK on a submit control fired the form's `submit` event and the
    /// page did not prevent it — the app should run the native GET/POST for
    /// this form. Carries the form + submitter arena nodes (the app maps them
    /// to its doc-model indices); unlike `SubmitDefault` the app hasn't
    /// pre-recorded which form, because the click path didn't know.
    SubmitForm { form: usize, submitter: usize },
}

#[derive(Debug)]
pub struct PageHandle {
    pub cmds: tokio::sync::mpsc::Sender<PageCmd>,
}

/// Wall budget for a single user-event dispatch's COMPUTE (a fetch fired
/// during it extends the deadline — see `DISPATCH_NET_GRACE`).
const DISPATCH_BUDGET: Duration = Duration::from_secs(1);

/// How long a dispatch may wait on in-flight network before giving up. A
/// page-initiated fetch during a dispatch pushes the deadline out to this, so a
/// click that asks an LLM for a reply waits for the (possibly slow, reasoning)
/// reply instead of being cancelled at the 1s compute cap. Generous so it works
/// with EVERY model; still bounded so a hung server can't hold the page forever
/// (the user can also Esc). The wait parks on the reactor — no idle CPU.
const DISPATCH_NET_GRACE: Duration = Duration::from_secs(300);
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
    // The actor keeps a clone of its own command sender so a WebSocket task can
    // post inbound frames back as `PageCmd::Ws` (see `setup_page_ws`).
    let cmd_self = cmd_tx.clone();
    let spawned = std::thread::Builder::new()
        .name(String::from("trust-page"))
        .stack_size(PAGE_STACK)
        .spawn(move || page_actor(html, env, cmd_rx, evt_tx, cmd_self));
    if spawned.is_err() {
        // The dropped evt sender tells the caller the page is gone.
    }
    (PageHandle { cmds: cmd_tx }, evt_rx)
}

/// Spawn the forwarder relaying a page's inbound WebSocket events into the
/// command stream as `PageCmd::Ws` (dispatched like a click). The `PageWs` host
/// object itself is registered earlier, in `load_page`, so a socket opened
/// during page load already has somewhere to deliver; this just connects its
/// event channel to the actor. No-op for a no-net page (no runtime handle).
fn setup_page_ws(
    page: &LoadedPage,
    mut ws_evt_rx: tokio::sync::mpsc::Receiver<(usize, crate::ws::WsIn)>,
    cmd_self: &tokio::sync::mpsc::Sender<PageCmd>,
) {
    let Some(handle) = page
        .ctx
        .realm()
        .host_defined()
        .get::<PageNet>()
        .map(|n| n.handle.clone())
    else {
        return;
    };
    // A WEAK sender: it must NOT keep the command channel open, or the actor's
    // `cmds.recv()` would never see the app drop the handle and the page would
    // never exit (a real deadlock the test suite caught). The forwarder upgrades
    // to send; once the app's (strong) sender is gone, upgrade fails and it stops.
    let weak = cmd_self.downgrade();
    handle.spawn(async move {
        while let Some((id, event)) = ws_evt_rx.recv().await {
            let Some(tx) = weak.upgrade() else { break };
            if tx.send(PageCmd::Ws { id, event }).await.is_err() {
                break; // actor gone
            }
        }
    });
}

fn page_actor(
    html: String,
    env: PageEnv,
    mut cmds: tokio::sync::mpsc::Receiver<PageCmd>,
    evts: tokio::sync::mpsc::Sender<PageEvt>,
    cmd_self: tokio::sync::mpsc::Sender<PageCmd>,
) {
    // Create the WS inbound channel up front so `PageWs` can be registered
    // DURING `load_page` (before any script runs) — a socket.io app opens its
    // WebSocket at load time and must find a live host immediately. The
    // forwarder that relays those events into the command stream is spawned
    // after load returns (it needs the actor's command sender).
    let (ws_evt_tx, ws_evt_rx) = tokio::sync::mpsc::channel::<(usize, crate::ws::WsIn)>(64);
    let mut page = match load_page(&html, &env, Some(ws_evt_tx)) {
        Ok(page) => page,
        Err(outcome) => {
            let _ = evts.blocking_send(PageEvt::Static { html, outcome });
            return;
        }
    };
    setup_page_ws(&page, ws_evt_rx, &cmd_self);
    // Drop our own strong command sender now: only the app's handle should keep
    // the channel open, so `cmds.recv()` returns `None` (and the actor exits)
    // the moment the app drops the page. The WS forwarder holds only a weak one.
    drop(cmd_self);
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
    // The shell render already delivered the errors accumulated so far — the
    // app reads them from `response.js` (the first event). `Updated` carries a
    // DELTA, so drain the reported errors here; otherwise the settle emit below
    // `take`s the SAME cumulative outcome and the app's `page_js_errors +=`
    // counts every load error twice (a single load error showed as `· JS:2!`).
    // Errors that arise DURING settle accumulate fresh and are reported once.
    // (Console is left cumulative — it's a diagnostic channel, not a count.)
    if painted_live {
        page.outcome.errors.clear();
    }

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
                // Clicking a submit control runs the form-submission algorithm.
                // If the page didn't prevent the `submit`, the app runs the
                // native GET/POST (a prevented submit falls through to the
                // re-render below — the page owns the update).
                if let Some((form, submitter)) = take_click_submit(&mut page) {
                    if evts
                        .blocking_send(PageEvt::SubmitForm { form, submitter })
                        .is_err()
                    {
                        return;
                    }
                    continue;
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
            PageCmd::Ws { id, event } => {
                dispatch_ws_in(&mut page, id, event);
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
                // A socket frame that mutated the DOM (a streamed chat token)
                // re-renders progressively, like any other dispatch.
                if !finish_dispatch(&mut page, &evts) {
                    return;
                }
            }
        }
    }
}

/// Deliver a WebSocket event to page JS (fire `open`/`message`/`close` on the
/// JS `WebSocket`), then settle — same shape as a click/form dispatch. A frame
/// is just an event; a streamed token mutates the DOM and re-renders.
fn dispatch_ws_in(page: &mut LoadedPage, id: usize, event: crate::ws::WsIn) {
    prepare_dispatch(page);
    let call = match event {
        crate::ws::WsIn::Open => format!("__trust.wsEvent({id},'open')"),
        crate::ws::WsIn::Text(s) => {
            format!("__trust.wsEvent({id},'message',{},false)", js_string(&s))
        }
        crate::ws::WsIn::Binary(b) => {
            // Deliver bytes as a latin1 string the prelude turns into an
            // ArrayBuffer/Blob per `binaryType`.
            let latin1: String = b.iter().map(|&x| x as char).collect();
            format!(
                "__trust.wsEvent({id},'message',{},true)",
                js_string(&latin1)
            )
        }
        crate::ws::WsIn::Closed { code, reason } => {
            // The socket is done: drop it from the registry so a `close`
            // handler can't resurrect a send, then fire the close event.
            if let Some(wsh) = page.ctx.realm().host_defined().get::<PageWs>() {
                wsh.sockets.borrow_mut().remove(&id);
            }
            format!(
                "__trust.wsEvent({id},'close','',false,{code},{})",
                js_string(&reason)
            )
        }
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        page.ctx.eval(Source::from_bytes(call.as_bytes()))
    })) {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => page.outcome.errors.push(format!("ws event: {err}")),
        Err(_) => {
            page.outcome.errors.push(String::from(
                "ws event: engine panic (Boa bug) — page JS halted",
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
    settle_image_loads(
        &mut page.ctx,
        &page.budget,
        DISPATCH_TICKS,
        &mut dispatch_outcome,
    );
    page.outcome.errors.extend(dispatch_outcome.errors);
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

/// After a click, whether it triggered an UN-PREVENTED form submit (a click
/// on a submit control whose `submit` event the page didn't cancel). Returns
/// the `(form, submitter)` arena nodes so the app runs the native GET/POST; a
/// prevented submit (page owns it) and a non-submit click both return None.
fn take_click_submit(page: &mut LoadedPage) -> Option<(usize, usize)> {
    let v = page
        .ctx
        .eval(Source::from_bytes(
            b"(function(){var s=__trust.lastClickSubmit;__trust.lastClickSubmit=null;\
              return (s && !s.prevented) ? (s.form + ',' + s.submitter) : '';})()",
        ))
        .ok()?;
    let s = v.to_string(&mut page.ctx).ok()?.to_std_string_lossy();
    let (f, sub) = s.split_once(',')?;
    Some((f.trim().parse().ok()?, sub.trim().parse().ok()?))
}

fn prepare_dispatch(page: &mut LoadedPage) {
    page.budget.rearm(DISPATCH_BUDGET);
    if let Some(net) = page.ctx.realm().host_defined().get::<PageNet>() {
        net.fetched.set(0);
        // We're in interactive dispatch mode now: a fetch fired from here on
        // extends the tight dispatch deadline so the wire can complete.
        net.dispatch.set(true);
    }
    // Give each fresh interaction its own MutationObserver loop budget: a
    // runaway observer chain in one dispatch disables delivery for the rest of
    // THAT window, but the next click starts clean.
    let _ = page.ctx.eval(Source::from_bytes(
        b"__trust.moResetGuard && __trust.moResetGuard()",
    ));
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
    // An interaction can mount images that reveal on `load` (clicking a
    // thumbnail opens a lightbox slide); fire their loads and settle the
    // reveal handlers so the image shows in this same dispatch.
    settle_image_loads(
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
    // An interaction can mount images that reveal on `load` (clicking a
    // thumbnail opens a lightbox slide); fire their loads and settle the
    // reveal handlers so the image shows in this same dispatch.
    settle_image_loads(
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
    // Delegated clicks on styled elements: a plain `<div class="enter">` with
    // no intrinsic clickable markup is still a click target when it advertises
    // interactivity with `cursor:pointer` AND a composed ancestor carries a
    // click listener. That's jQuery's `$(document).on('click', '.enter', fn)`
    // delegation — the selector lives in jQuery's closure where we can't read
    // it, but `cursor:pointer` is the library-agnostic affordance a sighted
    // user clicks, and the listening ancestor confirms a handler is waiting.
    let cursor_t = Instant::now();
    for &d in &everyone {
        if dom.computed_style(d, "cursor").as_deref() == Some("pointer") && listens(d) {
            candidates.insert(d);
        }
    }
    phase(&format!(
        "extract_live: cursor loop over {} nodes +{}ms",
        everyone.len(),
        cursor_t.elapsed().as_millis()
    ));
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
        ) || dom.is_contenteditable_host(d)
    });
    let has_any = !clickable.is_empty() || has_forms;
    let ser_t = Instant::now();
    let html = dom.serialize_live(crate::dom::DOCUMENT, &clickable);
    phase(&format!(
        "extract_live: serialize_live +{}ms",
        ser_t.elapsed().as_millis()
    ));
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
    // An interaction can mount images that reveal on `load` (clicking a
    // thumbnail opens a lightbox slide); fire their loads and settle the
    // reveal handlers so the image shows in this same dispatch.
    settle_image_loads(
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
        .filter(|(_, _, t, _)| is_classic(t))
        .filter_map(|(src, _, _, _)| src)
        .collect()
}

/// Does this page have scripts worth running at all?
pub fn has_scripts(html: &str) -> bool {
    !Dom::parse_document(html).scripts().is_empty()
}

/// Collect external stylesheet hrefs (raw attribute values, document
/// order) so the fetch pipeline can download them for the cascade.
/// De-duplicated by URL (keeping first-occurrence order): a page that links
/// the same sheet many times — GitHub references `primer-react-css` six times —
/// must not spend the fetch budget re-downloading it, which crowds out other
/// needed sheets (the cascade applies a sheet once regardless).
pub fn external_stylesheets(html: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    Dom::parse_document(html)
        .stylesheet_links()
        .into_iter()
        .filter(|href| seen.insert(href.clone()))
        .collect()
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

    // A <script> element runs when it is FIRST inserted into the document
    // (HTML "prepare a script") — the universal SDK-loader idiom
    // `document.body.appendChild(scriptEl)` (reCAPTCHA, lazy analytics, embeds).
    // Tracked so re-insertion never re-runs it (the spec's "already started"
    // flag). Classic scripts only; a non-JS `type` is left inert. Scripts
    // parsed from innerHTML do NOT execute (spec), so this only fires for
    // genuine element-node insertion through appendChild/insertBefore.
    const SCRIPTS_STARTED = new Set();
    function maybeRunScript(node) {
        if (!node || node.localName !== "script" || SCRIPTS_STARTED.has(node.__id)) return;
        const ty = (node.getAttribute("type") || "").trim().toLowerCase();
        if (ty && ty !== "text/javascript" && ty !== "application/javascript" && ty !== "text/ecmascript") return;
        // Only a script connected to the document runs (not one built up inside
        // a detached fragment, which executes when ITS root is later inserted).
        let n = node, connected = false;
        while (n) { if (n.nodeType === 9) { connected = true; break; } n = n.parentNode; }
        if (!connected) return;
        SCRIPTS_STARTED.add(node.__id);
        __dom_run_injected_script(node.__id);
    }

    // A freshly inserted <iframe>/<frame> connected to the document begins
    // loading (HTML "process the iframe attributes" runs on insertion). A frame
    // built up inside a detached fragment waits until its root is connected —
    // the next load/settle sweep (or a contentDocument read) realizes it then.
    // (Forward-referenced by the Node insert methods; `processIframeAttributes`
    // is hoisted alongside the other iframe helpers below.)
    function maybeProcessInsertedFrame(frame, parent) {
        if (!parent.isConnected) return;
        try { processIframeAttributes(frame); } catch (e) {}
    }

    // The document base URL: <base href> when present (archive.org sets
    // one; SPA routers resolve '.' against it), the page URL otherwise.
    // CACHED — a full querySelector("base[href]") on every .href/.src read was
    // ~5% of Steam's settle profile. The resolved base only changes on
    // navigation (location) or a runtime <base href> mutation/insertion, each of
    // which resets the cache (setLocParts; the `<base>` guards in setAttribute/
    // removeAttribute and the child-mutation methods). `null` = (re)compute; any
    // real resolved base is a non-null string (so "" stays a valid cache hit).
    // NOT tracked (self-heals on the next navigation, both vanishingly rare): a
    // `<base>` injected via innerHTML, or a case-variant setAttribute("HREF").
    let baseHrefCache = null;
    function baseHref() {
        if (baseHrefCache !== null) return baseHrefCache;
        const b = g.document.querySelector("base[href]");
        if (!b) return (baseHrefCache = g.location.href);
        const u = __url_parse(b.getAttribute("href") || "", g.location.href);
        return (baseHrefCache = u ? u[0] : g.location.href);
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
    // A headless DOM never decodes images, so the `load` event a real browser
    // fires when an image fetch succeeds never happens here. The ubiquitous
    // "reveal on load" idiom — an `<img>` painted at `opacity:0` (or hidden)
    // until a `load` handler reveals it (lightGallery's lightbox, lazy-loaders,
    // masonry, fade-in carousels) — then leaves the image invisible forever,
    // and the layout drops an `opacity:0` image entirely. We DO fetch and show
    // images in the layout/render pipeline, so optimistically firing `load` is
    // the correct default. Only imgs something is actually waiting on (a `load`
    // listener / `onload`) are fired, so an ordinary page pays nothing. The
    // event is deferred to a macrotask: a library inserts the `<img>` and THEN
    // binds its handler, so a browser fires `load` on the next turn, once the
    // handler is registered — we match that. Returns the count newly scheduled
    // so the actor can re-scan for images a load handler itself inserts
    // (lightGallery preloads the adjacent slides).
    trust.__imgLoaded = new Set();
    trust.scanImageLoads = function () {
        let imgs;
        try { imgs = g.document.querySelectorAll("img"); } catch (e) { return 0; }
        const pending = [];
        for (let i = 0; i < imgs.length; i++) {
            const im = imgs[i];
            const id = im.__id;
            if (typeof id !== "number" || trust.__imgLoaded.has(id)) continue;
            if (!im.getAttribute("src")) continue;
            const m = LS.get(im);
            const listening =
                (m && m.get("load") && m.get("load").length) || typeof im.onload === "function";
            if (!listening) continue;
            trust.__imgLoaded.add(id);
            pending.push(im);
        }
        if (pending.length) setTimeout(function () {
            for (const im of pending) { try { dispatch(im, new Event("load"), false); } catch (e) {} }
        }, 0);
        return pending.length;
    };
    // --- iframe processing: HTML "process the iframe attributes" ----------
    // An <iframe>/<frame> renders its nested document INLINE (the serializer
    // rewrites the frame + its realized content into a <div data-trust-frame>;
    // see dom.rs `frame_body`). Standards-faithful within the terminal: the
    // content navigable's document is fetched (src) or taken from the markup
    // (srcdoc), parsed as a REAL document, and its relative URLs are resolved
    // against the frame's own base. Deliberate, medium-forced deviations (her
    // calls): NO border or scrollbar chrome — the frame flows into the page's
    // single scroll; and the nested document's OWN scripts do NOT run (TRust
    // has one Boa realm per page, and a separate realm per nested navigable is
    // future engine work) — so a frame's HTML+CSS render but its in-frame JS
    // is inert. Cross-origin frames still RENDER but are not script-accessible
    // from the parent (`contentDocument` → null, per spec's origin check).
    function stripFragment(u) { const i = u.indexOf("#"); return i < 0 ? u : u.slice(0, i); }
    // A frame URL is same-origin with the page (about:blank/about:srcdoc
    // inherit the parent origin, so they count as same-origin).
    function frameSameOrigin(url) {
        if (!url || url === "about:srcdoc" || url === "about:blank") return true;
        const u = __url_parse(url, g.location.href);
        return u ? u[8] === g.location.origin : false;
    }
    // Shared attribute processing steps, step 3 — circular-navigation guard: a
    // frame must not load a URL already held by one of its inclusive ancestor
    // navigables (the infinite self-embed the spec forbids). The nested
    // document lives in the same arena, so the parentNode chain walks from the
    // frame up through every ancestor frame element to the top document.
    function frameAncestorHasUrl(frame, url) {
        const target = stripFragment(url);
        if (stripFragment(g.location.href) === target) return true;
        let n = frame.parentNode;
        while (n) {
            const ln = n.localName;
            if ((ln === "iframe" || ln === "frame") && n.__frameUrl &&
                stripFragment(n.__frameUrl) === target) return true;
            n = n.parentNode;
        }
        return false;
    }
    // "iframe load event steps": fire load at the element once its content
    // document has loaded. A macrotask so parent onload / addEventListener
    // handlers attached during the current turn still observe it (same shape
    // as the synthetic image-load pass).
    function fireFrameLoad(frame) {
        setTimeout(function () { try { dispatch(frame, new Event("load"), false); } catch (e) {} }, 0);
    }
    // Install markup as the frame's content navigable, then process any frames
    // nested inside it (bounded by the circular guard + the page fetch cap).
    function loadFrameMarkup(frame, markup, base, frameUrl) {
        frame.__frameUrl = frameUrl;
        __dom_load_frame(frame.__id, String(markup == null ? "" : markup), base);
        hydrateFramesIn(frame);
    }
    // "Process the iframe attributes". The initialInsertion / re-process cases
    // collapse into one idempotent function: the __loaded* de-dup makes a
    // repeat call for the SAME state a no-op, so the load sweep, the lazy
    // contentDocument getter, and src/srcdoc attribute changes all route here.
    function processIframeAttributes(frame) {
        if (!frame) return;
        const ln = frame.localName;
        if (ln !== "iframe" && ln !== "frame") return;
        // srcdoc takes priority over src (spec).
        const srcdoc = frame.getAttribute("srcdoc");
        if (srcdoc !== null) {
            if (frame.__loadedSrcdoc === srcdoc) return;
            frame.__loadedSrcdoc = srcdoc;
            frame.__loadedSrc = undefined;
            // about:srcdoc: the markup IS the document; base/origin inherit the
            // parent document.
            loadFrameMarkup(frame, srcdoc, g.location.href, "about:srcdoc");
            fireFrameLoad(frame);
            return;
        }
        frame.__loadedSrcdoc = undefined;
        // Shared attribute processing steps → a URL, or null (= about:blank).
        const src = frame.getAttribute("src");
        if (!src || src.trim() === "") { frame.__loadedSrc = undefined; return; }
        const parsed = __url_parse(src, baseHref());
        if (!parsed) return;
        const url = parsed[0];
        if (frame.__loadedSrc === url) return; // already navigated to this src
        // Only http(s) navigables are fetchable here (about:/data:/blob: render
        // nothing for now — a documented deviation).
        if (!/^https?:/i.test(url)) { frame.__loadedSrc = undefined; return; }
        if (frameAncestorHasUrl(frame, url)) return; // circular-navigation guard
        frame.__loadedSrc = url; // set before fetching so a re-sweep won't double-load
        let r;
        try { r = __http_fetch(url, "GET", null, null, null); } catch (e) { r = null; }
        if (!r) { fireFrameLoad(frame); return; }
        const status = r[0] | 0;
        const ctype = String(r[1] || "").toLowerCase();
        const isHtml = ctype === "" || ctype.indexOf("text/html") >= 0 ||
            ctype.indexOf("application/xhtml") >= 0;
        if (status >= 200 && status < 300 && isHtml) {
            loadFrameMarkup(frame, r[2] || "", url, url);
        }
        fireFrameLoad(frame);
    }
    // Process every frame within `root` (the document at load, or a freshly
    // installed frame document for nested frames). Idempotent (the __loaded*
    // de-dup), so re-sweeping is cheap.
    function hydrateFramesIn(root) {
        let frames;
        try { frames = root.querySelectorAll("iframe, frame"); } catch (e) { return 0; }
        for (let i = 0; i < frames.length; i++) {
            try { processIframeAttributes(frames[i]); } catch (e) {}
        }
        return frames.length;
    }
    // Lazy realization when a script reads a frame's contentDocument before the
    // load sweep (or for a frame inserted after load). The de-dup guards keep a
    // repeat call cheap; a frame with neither src nor srcdoc stays about:blank.
    function ensureFrameProcessed(frame) {
        if (frame.getAttribute("src") !== null || frame.getAttribute("srcdoc") !== null) {
            try { processIframeAttributes(frame); } catch (e) {}
        }
    }
    trust.hydrateFrames = function () { return hydrateFramesIn(g.document); };
    // The actor's entry points: dispatch a user click; enumerate nodes
    // with click listeners (delegation hosts included — the actor sorts
    // containers from buttons).
    // The submit control at or above `el` (the default action of clicking it
    // is to submit its form). A <button>'s type defaults to "submit";
    // type="button"/"reset" do not submit. <input type=submit|image> too.
    function submitControlFor(el) {
        let n = el;
        while (n && n.nodeType === 1) {
            const tag = n.localName;
            if (tag === "button") return (n.getAttribute("type") || "submit").toLowerCase() === "submit" ? n : null;
            if (tag === "input") { const ty = (n.getAttribute("type") || "").toLowerCase(); return (ty === "submit" || ty === "image") ? n : null; }
            n = n.parentNode;
        }
        return null;
    }
    trust.click = function (id) {
        trust.lastClickSubmit = null;
        const t = wrap(id);
        if (!t) return false;
        const ev = new Event("click", { bubbles: true, cancelable: true });
        dispatch(t, ev, false);
        if (ev.defaultPrevented) return true;
        // The default action of activating a submit control is to submit its
        // form (HTML). A live <button>/<input type=submit> reaches the app as a
        // JsClick, so without this a click fired only a `click` event and the
        // form's `submit` handler (e.g. React's onSubmit, bound on the <form>)
        // never ran — pixiv's login button did "nothing". Fire a real submit;
        // page JS may preventDefault (then it owns the update) — else the app
        // runs the native GET/POST.
        const btn = submitControlFor(t);
        if (btn) {
            const form = nearestForm(btn);
            if (form) {
                const sev = new Event("submit", { bubbles: true, cancelable: true });
                sev.submitter = btn;
                dispatch(form, sev, false);
                trust.lastClickSubmit = { form: form.__id, submitter: btn.__id, prevented: sev.defaultPrevented };
                return sev.defaultPrevented;
            }
        }
        return false;
    };
    // Fire a load/error event on an injected <script> (and its on<type>
    // handler), for loaders that wait on `script.onload` instead of polling.
    trust.scriptEvent = function (id, type) {
        const t = wrap(id);
        if (!t) return;
        const ev = new Event(type);
        dispatch(t, ev, false);
        const on = t["on" + type];
        if (typeof on === "function") { try { on.call(t, ev); } catch (e) { trust.errors.push("script on" + type + ": " + ((e && e.message) || e)); } }
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
    // A truthy `contenteditable` attribute marks an editing host (the editor
    // root). Mirrors `Dom::is_contenteditable_host` so both sides agree on which
    // element the edit targets.
    function ceHost(el) {
        if (!el || el.nodeType !== 1 || !el.hasAttribute("contenteditable")) return false;
        const v = (el.getAttribute("contenteditable") || "").trim().toLowerCase();
        return v === "" || v === "true" || v === "plaintext-only";
    }
    trust.formSet = function (id, value, checked) {
        const el = wrap(id);
        if (!el) return false;
        value = value === null || value === undefined ? "" : String(value);
        // A contenteditable host edits like a field but isn't a form control:
        // drive it with the real editing algorithm — a cancelable `beforeinput`,
        // then (unless the editor handled it) replace the content and fire
        // `input`. A rich editor (ProseMirror/TipTap) that preventDefaults owns
        // the change; a plain editable, or one that reconciles from DOM
        // mutations (its MutationObserver), takes our content + input event.
        if (ceHost(el)) {
            const bev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data: value });
            dispatch(el, bev, false);
            if (bev.defaultPrevented) return true;
            if (el.textContent === value) return false;
            el.textContent = value;
            dispatch(el, new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }), false);
            return true;
        }
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
        set textContent(v) {
            v = v === null || v === undefined ? "" : String(v);
            if (!MO.length) { __dom_set_text(this.__id, v); return; }
            const t = this.nodeType;
            if (t === 3 || t === 8) { const old = __dom_text(this.__id); __dom_set_text(this.__id, v); moCharData(this, old); return; }
            // On an element, textContent replaces all children with one text node.
            const removed = this.childNodes;
            __dom_set_text(this.__id, v);
            moChildBulk(this, removed, this.childNodes);
        }
        get nodeValue() { const t = this.nodeType; return t === 3 || t === 8 ? __dom_text(this.__id) : null; }
        set nodeValue(v) {
            const t = this.nodeType;
            if (t !== 3 && t !== 8) return;
            v = String(v);
            if (!MO.length) { __dom_set_text(this.__id, v); return; }
            const old = __dom_text(this.__id);
            __dom_set_text(this.__id, v);
            moCharData(this, old);
        }
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
            if (MO.length) moChildInsert(this, c);
            if (CE.defs.size) ceScan(c);
            maybeRunScript(c);
            if (c.__ln === "base") baseHrefCache = null; // maybeRunScript already read .localName
            else if (c.__ln === "iframe" || c.__ln === "frame") maybeProcessInsertedFrame(c, this);
            return c;
        }
        insertBefore(c, ref) {
            if (c && c.nodeType === 11 && !c.__host) { for (const k of c.childNodes) this.insertBefore(k, ref); return c; }
            __dom_insert_before(this.__id, c.__id, ref ? ref.__id : null);
            if (MO.length) moChildInsert(this, c);
            if (CE.defs.size) ceScan(c);
            maybeRunScript(c);
            if (c.__ln === "base") baseHrefCache = null;
            else if (c.__ln === "iframe" || c.__ln === "frame") maybeProcessInsertedFrame(c, this);
            return c;
        }
        removeChild(c) { if (c.__ln === "base") baseHrefCache = null; if (MO.length) moChildRemove(this, c); if (CE.defs.size) ceDisconnect(c); __dom_detach(c.__id); return c; }
        replaceChild(n, old) {
            const prev = old.previousSibling, next = old.nextSibling;
            if (CE.defs.size) ceDisconnect(old);
            __dom_insert_before(this.__id, n.__id, old.__id);
            __dom_detach(old.__id);
            if (MO.length) moNotify({ type: "childList", target: this, addedNodes: [n],
                removedNodes: [old], previousSibling: prev, nextSibling: next });
            if (CE.defs.size) ceScan(n);
            maybeRunScript(n);
            if (n.__ln === "base" || old.__ln === "base") baseHrefCache = null;
            else if (n.__ln === "iframe" || n.__ln === "frame") maybeProcessInsertedFrame(n, this);
            return old;
        }
        remove() { if (this.__ln === "base") baseHrefCache = null; const p = this.parentNode; if (p && MO.length) moChildRemove(p, this); if (CE.defs.size) ceDisconnect(this); __dom_detach(this.__id); }
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

    // Container types mpv routinely plays — what a media element honestly
    // reports it "can play" (we present media via mpv on follow, see layout's
    // `flow_media` + `is_playable_media_url`).
    const MEDIA_MIME = /^(?:video|audio)\/(?:mp4|webm|ogg|mpeg|mp3|aac|x-aac|x-m4a|mp4a-latm|flac|x-flac|wav|x-wav|x-matroska|quicktime|x-msvideo|x-flv|3gpp2?|x-ms-wmv)$/;
    function emptyTimeRanges() { return { length: 0, start() { return 0; }, end() { return 0; } }; }
    function emptyTrackList() { const l = []; l.getTrackById = () => null; l.addEventListener = () => {}; l.removeEventListener = () => {}; return l; }
    // The WebIDL brand for an HTML element, i.e. what
    // `Object.prototype.toString.call(el)` must report ("[object HTMLDivElement]").
    // Spec requires every platform object to carry its interface name as its
    // @@toStringTag; without it elements stringified as "[object Object]", which
    // broke the very common is-an-Element idiom `toString.call(x).includes("Element")`
    // (Tippy.js returns an empty array — and then `.destroy()` throws — when its
    // element check fails). Irregular tags map explicitly; hyphenated/generic tags
    // are the base HTMLElement; everything else is HTML<Cap>Element (which also
    // harmlessly names truly-unknown tags rather than tracking HTMLUnknownElement).
    const HTML_IFACE_IRREGULAR = {
        a: "Anchor", p: "Paragraph", ul: "UList", ol: "OList", li: "LI", dl: "DList",
        br: "BR", hr: "HR", img: "Image", q: "Quote", blockquote: "Quote",
        ins: "Mod", del: "Mod", caption: "TableCaption", col: "TableCol",
        colgroup: "TableCol", table: "Table", tbody: "TableSection",
        thead: "TableSection", tfoot: "TableSection", tr: "TableRow", td: "TableCell",
        th: "TableCell", textarea: "TextArea", iframe: "IFrame", frame: "Frame",
        frameset: "FrameSet", datalist: "DataList", optgroup: "OptGroup",
        fieldset: "FieldSet", h1: "Heading", h2: "Heading", h3: "Heading",
        h4: "Heading", h5: "Heading", h6: "Heading",
    };
    // Known elements with no specific interface (report the base HTMLElement).
    const HTML_IFACE_GENERIC = new Set(["abbr", "address", "article", "aside", "b",
        "bdi", "bdo", "cite", "code", "dd", "dfn", "dt", "em", "figcaption", "figure",
        "footer", "header", "hgroup", "i", "kbd", "main", "mark", "nav", "noscript",
        "rp", "rt", "ruby", "s", "samp", "section", "small", "strong", "sub",
        "summary", "sup", "u", "var", "wbr", "center", "acronym", "big", "nobr",
        "tt", "strike"]);
    function htmlInterfaceName(local) {
        const t = String(local || "").toLowerCase();
        if (!t) return "HTMLUnknownElement";
        if (t.indexOf("-") >= 0 || HTML_IFACE_GENERIC.has(t)) return "HTMLElement";
        const irr = HTML_IFACE_IRREGULAR[t];
        if (irr) return "HTML" + irr + "Element";
        return "HTML" + t.charAt(0).toUpperCase() + t.slice(1) + "Element";
    }
    // A real DOMTokenList (https://dom.spec.whatwg.org/#interface-domtokenlist)
    // backs `element.classList` (and `relList`). It MUST be a class with a
    // shared prototype, not a bare object literal: legacy classList polyfills
    // (W3Schools' common-deps, html5shiv-era shims) feature-detect a method on
    // an instance, then patch `DOMTokenList.prototype` — so the global has to
    // exist AND prototype patches have to reach every instance. The methods read
    // the live `class` attribute through `__el` so the list stays in sync with
    // direct `setAttribute("class", …)` writes.
    class DOMTokenList {
        constructor(el) { this.__el = el; }
        __get() { return (this.__el.getAttribute("class") || "").split(/\s+/).filter(Boolean); }
        __set(l) { this.__el.setAttribute("class", l.join(" ")); }
        add(...cs) { const l = this.__get(); for (const c of cs) if (!l.includes(String(c))) l.push(String(c)); this.__set(l); }
        remove(...cs) { const ss = cs.map(String); this.__set(this.__get().filter((x) => !ss.includes(x))); }
        toggle(c, force) {
            const has = this.__get().includes(String(c));
            const want = force === undefined ? !has : !!force;
            if (want && !has) this.add(c);
            if (!want && has) this.remove(c);
            return want;
        }
        replace(oldT, newT) {
            const l = this.__get(); const i = l.indexOf(String(oldT));
            if (i < 0) return false;
            if (!l.includes(String(newT))) l[i] = String(newT); else l.splice(i, 1);
            this.__set(l); return true;
        }
        contains(c) { return this.__get().includes(String(c)); }
        item(i) { return this.__get()[i] ?? null; }
        supports() { return true; }
        get length() { return this.__get().length; }
        get value() { return this.__el.getAttribute("class") || ""; }
        set value(v) { this.__el.setAttribute("class", String(v)); }
        toString() { return this.__el.getAttribute("class") || ""; }
        forEach(fn, thisArg) { this.__get().forEach((t, i) => fn.call(thisArg, t, i, this)); }
        keys() { return this.__get().keys(); }
        values() { return this.__get().values(); }
        entries() { return this.__get().entries(); }
        [Symbol.iterator]() { return this.__get()[Symbol.iterator](); }
    }

    class Element extends Node {
        // nodeType and the tag are IMMUTABLE for a node: `wrap()` already
        // dispatched this class BY node type, and an element's local name never
        // changes. So return a constant nodeType (no `__dom_node_type` syscall)
        // and lazily cache the tag — killing the per-access syscalls that
        // jQuery's `each`/`data`/`add` pound (profile-directed: `nodeType`/
        // `nodeName`/`tagName` getters were ~8% of Steam's settle phase; a
        // getter-hammering micro-bench runs ~25% faster).
        get nodeType() { return 1; }
        get localName() { let t = this.__ln; if (t === undefined) t = this.__ln = __dom_tag(this.__id) || ""; return t; }
        get tagName() { let t = this.__tn; if (t === undefined) t = this.__tn = this.localName.toUpperCase(); return t; }
        get nodeName() { return this.tagName; }
        get [Symbol.toStringTag]() { return htmlInterfaceName(this.localName); }
        // --- HTMLMediaElement surface, on <video>/<audio> ---
        // TRust presents media via mpv (a followed link), not inline playback.
        // But a player library (video.js, Plyr, JW Player, …) probes the
        // element and, finding it reports it can't play, shows "No compatible
        // source was found for this media" AND strips the <source> — so the
        // layout never sees the media to represent. Reporting honest support
        // for the formats mpv plays, plus benign media-element state, keeps the
        // <source> in the DOM and the error away. Gated to media tags so it
        // doesn't pollute other elements; general (helps every player lib).
        get __isMedia() {
            let m = this.__media;
            if (m === undefined) { const t = this.tagName; m = this.__media = (t === "VIDEO" || t === "AUDIO"); }
            return m;
        }
        canPlayType(type) {
            if (!this.__isMedia) return undefined;
            const t = String(type || "").toLowerCase().split(";")[0].trim();
            return MEDIA_MIME.test(t) || t === "application/x-mpegurl"
                || t === "application/vnd.apple.mpegurl" || t === "application/dash+xml"
                ? "maybe" : "";
        }
        load() {}
        play() { return Promise.resolve(); }
        pause() {}
        addTextTrack() { return { mode: "disabled", cues: null, activeCues: null, addCue() {}, removeCue() {}, addEventListener() {}, removeEventListener() {} }; }
        fastSeek(t) { if (this.__isMedia) this.__ct = +t || 0; }
        get readyState() { return this.__isMedia ? 0 : undefined; }
        get networkState() { return this.__isMedia ? 0 : undefined; }
        get error() { return this.__isMedia ? null : undefined; }
        get ended() { return this.__isMedia ? false : undefined; }
        get seeking() { return this.__isMedia ? false : undefined; }
        get duration() { return this.__isMedia ? NaN : undefined; }
        get videoWidth() { return this.__isMedia ? 0 : undefined; }
        get videoHeight() { return this.__isMedia ? 0 : undefined; }
        get currentSrc() { return this.__isMedia ? (this.getAttribute("src") || "") : undefined; }
        get buffered() { return this.__isMedia ? emptyTimeRanges() : undefined; }
        get played() { return this.__isMedia ? emptyTimeRanges() : undefined; }
        get seekable() { return this.__isMedia ? emptyTimeRanges() : undefined; }
        get textTracks() { return this.__isMedia ? (this.__tt || (this.__tt = emptyTrackList())) : undefined; }
        get audioTracks() { return this.__isMedia ? (this.__at || (this.__at = emptyTrackList())) : undefined; }
        get videoTracks() { return this.__isMedia ? (this.__vt || (this.__vt = emptyTrackList())) : undefined; }
        get paused() { return this.__isMedia ? (this.__paused !== false) : undefined; }
        set paused(v) { this.__paused = !!v; }
        get currentTime() { return this.__isMedia ? (this.__ct || 0) : undefined; }
        set currentTime(v) { this.__ct = +v || 0; }
        get volume() { return this.__isMedia ? (this.__vol === undefined ? 1 : this.__vol) : undefined; }
        set volume(v) { this.__vol = +v; }
        get muted() { return this.__isMedia ? !!this.__muted : undefined; }
        set muted(v) { this.__muted = !!v; }
        get playbackRate() { return this.__isMedia ? (this.__pbr === undefined ? 1 : this.__pbr) : undefined; }
        set playbackRate(v) { this.__pbr = +v; }
        get defaultPlaybackRate() { return this.__isMedia ? 1 : undefined; }
        set defaultPlaybackRate(_v) {}
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
        // getAttribute is hammered by every framework's traversal/normalisation
        // (jQuery's .attr/.hasClass, event delegation, and the value/checked/id/
        // class IDL getters below all route here). A per-element read cache
        // (`__ac`) elides the repeat `__dom_get_attr` syscalls. It's a null-proto
        // bag so attribute names like "constructor"/"__proto__" stay plain keys;
        // the syscall only ever returns a string or null, so a cached `undefined`
        // uniquely means "not cached yet".
        // CORRECTNESS: every attribute write on the live page funnels through
        // setAttribute/removeAttribute (style/dataset/classList/className/value/
        // checked all route here; nothing mutates an attr Rust-side mid-page), so
        // those two are the only invalidation points. Because Rust matches
        // attribute names case-INSENSITIVELY, a write DROPS the whole bag instead
        // of patching one raw-cased key (cheap, and immune to mixed-case access).
        getAttribute(n) {
            n = String(n);
            const c = this.__ac || (this.__ac = Object.create(null));
            const v = c[n];
            if (v !== undefined) return v;
            return (c[n] = __dom_get_attr(this.__id, n));
        }
        setAttribute(n, v) {
            n = String(n); v = String(v);
            const old = (this.__ceUpgraded || MO.length) ? this.getAttribute(n) : null;
            __dom_set_attr(this.__id, n, v);
            this.__ac = undefined; // attrs changed: drop the read cache (see getAttribute)
            if (n === "href" && this.localName === "base") baseHrefCache = null;
            ceAttrChanged(this, n.toLowerCase(), old, v);
            if (MO.length) moAttr(this, n, old);
            // Changing src/srcdoc re-runs "process the iframe attributes".
            if (n === "src" || n === "srcdoc") { const ln = this.localName; if (ln === "iframe" || ln === "frame") processIframeAttributes(this); }
        }
        setAttributeNS(_, n, v) { this.setAttribute(n, v); }
        removeAttribute(n) {
            n = String(n);
            const old = (this.__ceUpgraded || MO.length) ? this.getAttribute(n) : null;
            __dom_remove_attr(this.__id, n);
            this.__ac = undefined; // attrs changed: drop the read cache (see getAttribute)
            if (n === "href" && this.localName === "base") baseHrefCache = null;
            ceAttrChanged(this, n.toLowerCase(), old, null);
            if (MO.length) moAttr(this, n, old);
            // Removing src/srcdoc re-runs "process the iframe attributes".
            if (n === "src" || n === "srcdoc") { const ln = this.localName; if (ln === "iframe" || ln === "frame") processIframeAttributes(this); }
        }
        hasAttribute(n) { return this.getAttribute(n) !== null; }
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
                const attr = {
                    name: n, localName: n, nodeName: n, namespaceURI: null,
                    prefix: null, specified: true, ownerElement: this,
                    value: v, nodeValue: v,
                };
                list.push(attr);
                // NamedNodeMap named-property access: `attributes[name]` returns
                // the Attr (WebIDL named getter). jQuery's event-support probe
                // reads `div.attributes["onsubmit"].expando`; without this it
                // was undefined → ToObject throw that aborted jQuery's boot.
                // Skip names that would clobber the array length / methods.
                if (n !== "length" && n !== "item" && n !== "getNamedItem") list[n] = attr;
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
        get value() {
            const ln = this.localName;
            // <select>.value is the value of its first selected option (HTML
            // spec), not a `value` content attribute — selects have none.
            if (ln === "select") { const o = this.__selectedOption(); return o ? o.value : ""; }
            // <option>.value falls back to its text when the attribute is absent
            // (matches the form-submit option logic), so a valueless <option>
            // still round-trips its label.
            if (ln === "option") { const v = this.getAttribute("value"); return v === null ? this.textContent : v; }
            // <textarea> has NO `value` content attribute — its value is its raw
            // text content (HTML spec), which is also what the form submit path
            // and `formSet` read/write. Reading the (always-absent) attribute
            // returned "" and broke every "grab the textarea, do something with
            // its text" script — the W3Schools tryit editor writes
            // `textarea.value` into its result iframe, so an empty read rendered
            // a blank result.
            if (ln === "textarea") return this.textContent;
            const v = this.getAttribute("value"); return v === null ? "" : v;
        }
        set value(v) {
            if (this.localName === "select") { this.__selectValue(String(v)); return; }
            if (this.localName === "textarea") { this.textContent = String(v); return; }
            this.setAttribute("value", String(v));
        }
        // --- HTMLSelectElement surface (options/index/multiple) ---
        // `options` is the select's <option> descendants (optgroups included, per
        // spec) as a real Array — `.length`/`[i]` and `.filter`/iteration all
        // work natively, like every other collection the prelude hands back.
        __options() { return this.localName === "select" ? this.querySelectorAll("option") : []; }
        __selectedOption() {
            const os = this.__options();
            for (const o of os) if (o.selected) return o;
            // A single (non-multiple) select with nothing explicitly selected
            // defaults to its first option (HTML spec).
            return (!this.multiple && os.length) ? os[0] : null;
        }
        __selectValue(val) {
            const os = this.__options(); let matched = false;
            for (const o of os) {
                const m = !matched && o.value === val;
                o.selected = m;
                if (m) matched = true;
            }
        }
        get options() { return this.localName === "select" ? this.__options() : undefined; }
        get selectedOptions() { return this.localName === "select" ? this.__options().filter((o) => o.selected) : undefined; }
        get selectedIndex() {
            if (this.localName !== "select") return undefined;
            const os = this.__options();
            for (let i = 0; i < os.length; i++) if (os[i].selected) return i;
            return this.multiple ? -1 : (os.length ? 0 : -1);
        }
        set selectedIndex(i) {
            if (this.localName !== "select") return;
            const os = this.__options(); i = Number(i);
            for (let k = 0; k < os.length; k++) os[k].selected = (k === i);
        }
        get multiple() { return this.hasAttribute("multiple"); }
        set multiple(v) { if (v) this.setAttribute("multiple", ""); else this.removeAttribute("multiple"); }
        get checked() { return this.hasAttribute("checked"); }
        set checked(v) { if (v) this.setAttribute("checked", ""); else this.removeAttribute("checked"); }
        get disabled() { return this.hasAttribute("disabled"); }
        set disabled(v) { if (v) this.setAttribute("disabled", ""); else this.removeAttribute("disabled"); }
        // HTMLOptionElement.selected / .defaultSelected. A headless DOM has no
        // separate "dirty selectedness", so both reflect the `selected` content
        // attribute — which is ALSO what the layout/form code reads to know
        // which option is current (`option[selected]`). React's <select> commit
        // (`postMountWrapper`/`updateOptions`) reads+writes both on every option,
        // so getter-only would throw in strict mode. (`option.disabled` already
        // works above; it's read in the same loop.)
        get selected() { return this.hasAttribute("selected"); }
        set selected(v) { if (v) this.setAttribute("selected", ""); else this.removeAttribute("selected"); }
        get defaultSelected() { return this.hasAttribute("selected"); }
        set defaultSelected(v) { if (v) this.setAttribute("selected", ""); else this.removeAttribute("selected"); }
        get hidden() { return this.hasAttribute("hidden"); }
        set hidden(v) { if (v) this.setAttribute("hidden", ""); else this.removeAttribute("hidden"); }
        // Reflected string IDL attributes (HTML spec): the getter returns the
        // content attribute or "" — NOT undefined — so the universal idiom
        // `el.lang.toLowerCase()` / `el.dir === "rtl"` works. pixiv reads
        // `document.documentElement.lang.toLowerCase()` at boot; without this
        // it got `undefined.toLowerCase()` and threw, killing the whole bundle.
        get lang() { return this.getAttribute("lang") || ""; }
        set lang(v) { this.setAttribute("lang", String(v)); }
        get dir() { return this.getAttribute("dir") || ""; }
        set dir(v) { this.setAttribute("dir", String(v)); }
        get title() { return this.getAttribute("title") || ""; }
        set title(v) { this.setAttribute("title", String(v)); }
        get slot() { return this.getAttribute("slot") || ""; }
        set slot(v) { this.setAttribute("slot", String(v)); }
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
            if (!MO.length) { __dom_set_inner_html(this.__id, String(v)); if (CE.defs.size) ceScan(this); return; }
            const removed = this.childNodes;
            __dom_set_inner_html(this.__id, String(v));
            moChildBulk(this, removed, this.childNodes);
            if (CE.defs.size) ceScan(this);
        }
        get content() {
            // <template>.content: the inert fragment its markup parses into.
            if (this.localName === "template") return wrap(__dom_template_content(this.__id));
            // HTMLMetaElement.content reflects the `content` attribute (HTML
            // spec). pixiv stashes its boot config as JSON in
            // `<meta id="meta-global-data" content='{…}'>` and does
            // `JSON.parse(meta.content)`; without reflection that was an
            // expando `undefined` → `JSON.parse(undefined)` → SyntaxError.
            if (this.localName === "meta") return this.getAttribute("content") || "";
            return this.__content;
        }
        // Non-template, non-meta elements have NO `content` property in the DOM,
        // so a framework's `.content=${…}` property binding (lit's PropertyPart
        // does `element[name] = value`) just sets a plain expando — it must NOT
        // throw the way a getter-without-setter does in strict mode. Templates
        // keep their read-only fragment; <meta> reflects its attribute.
        set content(v) {
            if (this.localName === "template") return;
            if (this.localName === "meta") { this.setAttribute("content", String(v)); return; }
            this.__content = v;
        }
        // HTMLIFrameElement.contentDocument / .contentWindow — the nested
        // browsing context's document and WindowProxy. Backed by a real
        // same-arena `FrameDocument` (see that class): the near-universal idiom
        // `iframe.contentDocument || iframe.contentWindow.document` (analytics
        // beacons, sandboxed injectors, code-playground panes, the W3Schools
        // tryit editor) reads them unconditionally, and now content written via
        // `document.write`/`srcdoc` actually renders inline. Cached on the
        // (identity-stable) wrapper. Non-frame elements keep returning undefined.
        get contentDocument() {
            if (this.localName !== "iframe" && this.localName !== "frame") return undefined;
            ensureFrameProcessed(this); // load src/srcdoc if a script reads us early
            // A cross-origin nested document RENDERS but isn't script-accessible
            // from the parent (spec's origin check) — hand back null, as a real
            // browser does.
            if (this.__frameUrl && !frameSameOrigin(this.__frameUrl)) return null;
            return this.__contentDoc || (this.__contentDoc = new FrameDocument(this));
        }
        get contentWindow() {
            if (this.localName !== "iframe" && this.localName !== "frame") return undefined;
            if (!this.__contentWin) {
                const frame = this;
                this.__contentWin = {
                    get document() { return frame.contentDocument; },
                    // The nested document's location reflects the navigated URL
                    // (about:blank until a src/srcdoc loads).
                    get location() {
                        const u = frame.__frameUrl;
                        return { href: u && u !== "about:srcdoc" ? u : "about:blank", replace() {}, assign() {} };
                    },
                    parent: g, top: g, frames: g, frameElement: this,
                    postMessage() {}, focus() {}, blur() {},
                    addEventListener() {}, removeEventListener() {},
                };
                this.__contentWin.self = this.__contentWin;
                this.__contentWin.window = this.__contentWin;
            }
            return this.__contentWin;
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
            p = String(p).toLowerCase();
            const container = (p === "beforebegin" || p === "afterend") ? this.parentNode : this;
            if (!MO.length || !container) {
                __dom_insert_adjacent(this.__id, p, String(h));
                if (CE.defs.size) { const par = this.parentNode; ceScan(par || this); }
                return;
            }
            const before = new Set(container.childNodes.map((k) => k.__id));
            __dom_insert_adjacent(this.__id, p, String(h));
            const added = container.childNodes.filter((k) => !before.has(k.__id));
            moChildBulk(container, [], added);
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
            if (!this.__cl) this.__cl = new DOMTokenList(this);
            return this.__cl;
        }
        matches(s) { return !!__dom_matches(this.__id, String(s)); }
        webkitMatchesSelector(s) { return this.matches(s); }
        closest(s) { let e = this; while (e && e.nodeType === 1) { if (e.matches(s)) return e; e = e.parentNode; } return null; }
        click() {} focus() {} blur() {} scrollIntoView() {}
        // Element scrolling (CSSOM View) is a no-op here — the terminal scrolls
        // the laid-out document, not individual DOM boxes. They MUST exist as
        // callable methods, though: a chat/feed that auto-scrolls its container
        // on new content calls `el.scrollTo(...)` inside a framework effect, and
        // a missing method throws — which in Svelte 5 aborts the whole effect-
        // flush batch, so a sibling effect (the one rendering the new content)
        // silently never runs (Open WebUI's streamed reply rendered nothing).
        scrollTo() {} scrollBy() {} scroll() {}
        // Geometry: a layout pass over the live DOM gives each element its REAL
        // box (CSS pixels, quantized to terminal cells — what we actually
        // paint). `__dom_rect` returns [left, top, width, height] for a laid-out
        // element, or null when it has none (un-rendered / detached /
        // display:none), in which case we keep the generous viewport-box
        // fallback so measurement gates ("render once I have a non-zero size")
        // still fire. Coordinates are document-origin (page scroll is not
        // threaded in yet, so they read viewport-relative at the top of the
        // page, where load-time measurement happens).
        __rect() {
            let r = null;
            try { r = __dom_rect(this.__id); } catch (e) { r = null; }
            if (r) {
                const left = r[0], top = r[1], width = r[2], height = r[3];
                return { x: left, y: top, left, top, width, height,
                         right: left + width, bottom: top + height,
                         toJSON() { return this; } };
            }
            return { x: 0, y: 0, left: 0, top: 0, right: g.innerWidth, bottom: g.innerHeight,
                     width: g.innerWidth, height: g.innerHeight, toJSON() { return this; } };
        }
        getBoundingClientRect() { return this.__rect(); }
        getClientRects() { return [this.__rect()]; }
        get offsetWidth() { return this.__rect().width; }
        get offsetHeight() { return this.__rect().height; }
        get offsetTop() { return this.__rect().top; }
        get offsetLeft() { return this.__rect().left; }
        get offsetParent() { return g.document.body; }
        get clientWidth() { return this.__rect().width; }
        get clientHeight() { return this.__rect().height; }
        get clientTop() { return 0; }
        get clientLeft() { return 0; }
        get scrollWidth() { return this.__rect().width; }
        get scrollHeight() { return this.__rect().height; }
        get scrollTop() { return 0; }
        set scrollTop(_v) {}
        get scrollLeft() { return 0; }
        set scrollLeft(_v) {}
    }

    // CharacterData: the shared text-bearing interface for Text and Comment.
    // `data` is [LegacyNullToEmptyString] — null becomes "" (but undefined
    // stringifies to "undefined"); `length` is the data's UTF-16 length.
    class CharacterData extends Node {
        get data() { return __dom_text(this.__id) || ""; }
        // The single choke point for text/comment data changes: `data`,
        // `nodeValue`, `appendData`/`insertData`/`deleteData`/`replaceData`, and
        // Node's `textContent` (on a text node) all route here, so the
        // characterData MutationRecord is emitted from this one setter.
        set data(v) {
            v = v === null ? "" : String(v);
            if (!MO.length) { __dom_set_text(this.__id, v); return; }
            const old = __dom_text(this.__id) || "";
            __dom_set_text(this.__id, v);
            moCharData(this, old);
        }
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
    class Text extends CharacterData { get nodeType() { return 3; } get nodeName() { return "#text"; } get [Symbol.toStringTag]() { return "Text"; } }

    class Document extends Node {
        get nodeType() { return 9; }
        get nodeName() { return "#document"; }
        get [Symbol.toStringTag]() { return "HTMLDocument"; }
        // A document has NO owner document (DOM §`Document` overrides Node's
        // `ownerDocument` to null). Inheriting Node's "return the document" here
        // made `document.ownerDocument === document`, which sent ProseMirror's
        // shadow-root getSelection shim (`() => n.ownerDocument.getSelection()`)
        // into infinite recursion when it patched a missing `document.getSelection`.
        get ownerDocument() { return null; }
        // `document.getSelection()` is the Selection API alias for
        // `window.getSelection()`. Without it ProseMirror monkey-patches the
        // root's prototype to add one — see `ownerDocument` above.
        getSelection() { return g.getSelection(); }
        get documentElement() { return wrap(__dom_doc_element()); }
        get body() { return this.querySelector("body"); }
        get head() { return this.querySelector("head"); }
        get readyState() { return trust.readyState; }
        get title() { const t = this.querySelector("title"); return t ? t.textContent : ""; }
        set title(v) { const t = this.querySelector("title"); if (t) t.textContent = String(v); }
        get cookie() { return __cookie_get(); }
        set cookie(v) { __cookie_set(String(v)); }
        get location() { return g.location; }
        // The document's origin domain. Spec returns the origin's effective
        // domain (the host); a terminal browser has no frames, so the host IS
        // the domain. The setter is the legacy same-origin relaxation — store
        // it so a `document.domain = document.domain` round-trips, but it has
        // no cross-origin effect here. Missing this throws on sites that read
        // it (GitHub's behaviors bundle: "Unable to get document domain").
        get domain() { return this.__domain !== undefined ? this.__domain : g.location.hostname; }
        set domain(v) { this.__domain = String(v); }
        get defaultView() { return g; }
        get documentURI() { return g.location.href; }
        get URL() { return g.location.href; }
        get currentScript() { return wrap(trust.currentScript); }
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
                        createTreeWalker: (r, w, f) => new TreeWalker(r, w, f),
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
        createTreeWalker(root, whatToShow, filter) { return new TreeWalker(root, whatToShow, filter); }
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

    // HTMLIFrameElement's nested-browsing-context document — the part of the
    // iframe spec a terminal can honor: same-origin scripted/`srcdoc` content.
    // (https://html.spec.whatwg.org/multipage/iframe-embed-object.html). The
    // nested document is built from REAL arena nodes parented under the
    // <iframe> element (<html><head><body>), so `document.open/write/close`
    // and DOM mutations land in the live tree, the CSS cascade sees the frame's
    // own <style>, and the serializer can flow the body inline (it rewrites the
    // <iframe>+content into a block so the re-parse doesn't treat it as the
    // RAWTEXT the HTML parser makes of <iframe> content). A cross-origin `src`
    // frame we never load keeps an empty body and renders nothing — the same
    // graceful degrade as before.
    class FrameDocument {
        constructor(frameEl) {
            this.__frame = frameEl;
            this.nodeType = 9;
        }
        // The content navigable's document element, found live in the arena.
        // `processIframeAttributes` installs real <html> content for src/srcdoc
        // frames; an unscripted about:blank frame gets an empty skeleton on
        // first access so `document.write` has a <body> to write into. (Found,
        // not cached, so it stays correct after a (re)navigation replaces it.)
        get documentElement() {
            const kids = this.__frame.childNodes;
            for (let i = 0; i < kids.length; i++) {
                const c = kids[i];
                if (c.nodeType === 1 && c.localName === "html") return c;
            }
            const html = document.createElement("html");
            html.appendChild(document.createElement("head"));
            html.appendChild(document.createElement("body"));
            this.__frame.appendChild(html);
            return html;
        }
        get head() { return this.documentElement.querySelector("head") || this.documentElement; }
        get body() { return this.documentElement.querySelector("body") || this.documentElement; }
        get defaultView() { return this.__frame.contentWindow; }
        get readyState() { return "complete"; }
        get cookie() { return ""; }
        set cookie(_v) {}
        get title() { const t = this.querySelector("title"); return t ? t.textContent : ""; }
        set title(v) { let t = this.querySelector("title"); if (!t) { t = this.createElement("title"); this.head.appendChild(t); } t.textContent = String(v); }
        get location() { return this.__frame.contentWindow.location; }
        get implementation() { return document.implementation; }
        get [Symbol.toStringTag]() { return "HTMLDocument"; }
        open() { const b = this.body; while (b.firstChild) b.removeChild(b.firstChild); return this; }
        write(s) { this.body.insertAdjacentHTML("beforeend", String(s)); }
        writeln(s) { this.write(s + "\n"); }
        close() {}
        createElement(t) { return document.createElement(t); }
        createElementNS(_n, t) { return document.createElement(t); }
        createTextNode(s) { return document.createTextNode(s); }
        createComment(s) { return document.createComment(s); }
        createDocumentFragment() { return document.createDocumentFragment(); }
        createEvent(t) { return document.createEvent(t); }
        createRange() { return new Range(); }
        getElementById(i) { return this.documentElement.querySelector('[id="' + String(i).replace(/"/g, '\\"') + '"]'); }
        getElementsByTagName(t) { return this.documentElement.getElementsByTagName(t); }
        getElementsByClassName(c) { return this.documentElement.querySelectorAll("." + String(c)); }
        getElementsByName(n) { return this.documentElement.querySelectorAll('[name="' + String(n).replace(/"/g, '\\"') + '"]'); }
        querySelector(s) { return this.documentElement.querySelector(s); }
        querySelectorAll(s) { return this.documentElement.querySelectorAll(s); }
        addEventListener() {} removeEventListener() {} dispatchEvent() { return true; }
        hasFocus() { return true; }
    }

    class DocumentFragment extends Node { get nodeType() { return 11; } get nodeName() { return "#document-fragment"; } get [Symbol.toStringTag]() { return "DocumentFragment"; } }
    class Comment extends CharacterData { get nodeType() { return 8; } get nodeName() { return "#comment"; } get [Symbol.toStringTag]() { return "Comment"; } }
    // Lit walks comment markers with one of these.
    // A spec-faithful DOM TreeWalker (https://dom.spec.whatwg.org/#interface-treewalker).
    // The full traversal surface — firstChild/lastChild/next|previousSibling/
    // next|previousNode/parentNode — and the NodeFilter (whatToShow bitmask +
    // the acceptNode callback) are required: Primer React's focus zone builds
    // `createTreeWalker(root, SHOW_ELEMENT, {acceptNode})` and drives it with
    // `firstChild()`/`nextNode()` to find focusable elements. A walker missing
    // those methods threw `… is not a callable (reading 'firstChild')` during
    // React render, which GitHub's top-level boundary turned into "Unable to
    // load page." (FILTER_ACCEPT=1, FILTER_REJECT=2, FILTER_SKIP=3.)
    class TreeWalker {
        constructor(root, whatToShow, filter) {
            this.root = root;
            this.currentNode = root;
            this.whatToShow = (whatToShow === undefined ? 0xFFFFFFFF : whatToShow) >>> 0;
            this.filter = filter || null;
        }
        __filter(n) {
            const t = n.nodeType;
            const bit = (t >= 1 && t <= 32) ? (1 << (t - 1)) : 0;
            if ((this.whatToShow & bit) === 0) return 3; // FILTER_SKIP
            const f = this.filter;
            if (f === null) return 1; // FILTER_ACCEPT
            return typeof f === "function" ? f(n) : f.acceptNode(n);
        }
        // "traverse children", first=true -> firstChild(), false -> lastChild().
        __children(first) {
            let node = first ? this.currentNode.firstChild : this.currentNode.lastChild;
            while (node) {
                const result = this.__filter(node);
                if (result === 1) { this.currentNode = node; return node; }
                if (result === 3) {
                    const child = first ? node.firstChild : node.lastChild;
                    if (child) { node = child; continue; }
                }
                while (node) {
                    const sibling = first ? node.nextSibling : node.previousSibling;
                    if (sibling) { node = sibling; break; }
                    const parent = node.parentNode;
                    if (!parent || parent === this.root || parent === this.currentNode) return null;
                    node = parent;
                }
            }
            return null;
        }
        firstChild() { return this.__children(true); }
        lastChild() { return this.__children(false); }
        // "traverse siblings", next=true -> nextSibling(), false -> previousSibling().
        __siblings(next) {
            let node = this.currentNode;
            if (node === this.root) return null;
            for (;;) {
                let sibling = next ? node.nextSibling : node.previousSibling;
                while (sibling) {
                    node = sibling;
                    const result = this.__filter(node);
                    if (result === 1) { this.currentNode = node; return node; }
                    sibling = next ? node.firstChild : node.lastChild;
                    if (result === 2 || !sibling) sibling = next ? node.nextSibling : node.previousSibling;
                }
                node = node.parentNode;
                if (!node || node === this.root) return null;
                if (this.__filter(node) === 1) return null;
            }
        }
        nextSibling() { return this.__siblings(true); }
        previousSibling() { return this.__siblings(false); }
        parentNode() {
            let node = this.currentNode;
            while (node && node !== this.root) {
                node = node.parentNode;
                if (node && this.__filter(node) === 1) { this.currentNode = node; return node; }
            }
            return null;
        }
        nextNode() {
            let node = this.currentNode;
            let result = 1; // FILTER_ACCEPT
            for (;;) {
                while (result !== 2 && node.firstChild) {
                    node = node.firstChild;
                    result = this.__filter(node);
                    if (result === 1) { this.currentNode = node; return node; }
                }
                let temporary = node;
                let broke = false;
                while (temporary) {
                    if (temporary === this.root) return null;
                    const sibling = temporary.nextSibling;
                    if (sibling) { node = sibling; broke = true; break; }
                    temporary = temporary.parentNode;
                }
                if (!broke) return null;
                result = this.__filter(node);
                if (result === 1) { this.currentNode = node; return node; }
            }
        }
        previousNode() {
            let node = this.currentNode;
            while (node !== this.root) {
                let sibling = node.previousSibling;
                while (sibling) {
                    node = sibling;
                    let result = this.__filter(node);
                    while (result !== 2 && node.lastChild) {
                        node = node.lastChild;
                        result = this.__filter(node);
                    }
                    if (result === 1) { this.currentNode = node; return node; }
                    sibling = node.previousSibling;
                }
                if (node === this.root || !node.parentNode) return null;
                node = node.parentNode;
                if (this.__filter(node) === 1) { this.currentNode = node; return node; }
            }
            return null;
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
                createTreeWalker: (r, w, f) => new TreeWalker(r, w, f),
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
        get nodeType() { return 11; }
        get nodeName() { return "#document-fragment"; }
        get [Symbol.toStringTag]() { return "ShadowRoot"; }
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
        if (!node || typeof node !== "object" || node.__id === undefined) return;
        // Rust returns just the custom-element candidates (hyphenated tags) in
        // the inserted subtree, shadow roots included and the root itself — so
        // we wrap/visit only those, never the non-custom bulk of the subtree
        // (the old per-node JS recursion wrapped every node it walked).
        const ids = __dom_ce_candidates(node.__id);
        for (let i = 0; i < ids.length; i++) {
            const el = wrap(ids[i]);
            const ctor = CE.defs.get(el.localName);
            if (ctor) { upgradeElement(el, ctor); maybeConnect(el); }
        }
    }
    // define()'s catch-up upgrade, but shadow-piercing: an element
    // rendered into a shadow root BEFORE its definition (archive.org's
    // router does this for the late-loaded page component) is invisible
    // to document.querySelectorAll, so without crossing __sr it would
    // never upgrade — constructed never, rendered never, empty forever.
    function ceUpgradeName(name, ctor) {
        // The candidate set — every composed-tree element with this tag, shadow
        // roots included — is computed in Rust in a single pointer walk (see
        // __dom_upgrade_candidates) instead of recursing the whole document in
        // JS on every define(). Only the matching elements are wrapped and
        // upgraded; the old walk materialized a wrapper + a childNodes syscall
        // for ALL ~16.8k nodes per define on a big page.
        const ids = __dom_upgrade_candidates(g.document.__id, name);
        for (let i = 0; i < ids.length; i++) upgradeElement(wrap(ids[i]), ctor);
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
            ceUpgradeName(name, ctor);
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
    class SVGElement extends Element { get [Symbol.toStringTag]() { return "SVGElement"; } }
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
    // SVG element interface zoo (all extend SVGElement). SvelteKit's link
    // handler branches on `e instanceof SVGAElement` to read `href.baseVal`
    // vs `href` — a bare `SVGAElement` was a ReferenceError that broke its
    // link interception/preloading; libraries also feature-detect these.
    for (const __n of ["A", "SVG", "G", "Defs", "Desc", "Title", "Symbol", "Use",
        "Image", "Switch", "Style", "Script", "Path", "Rect", "Circle", "Ellipse",
        "Line", "Polyline", "Polygon", "Text", "TSpan", "TextPath", "Marker",
        "ClipPath", "Mask", "Pattern", "LinearGradient", "RadialGradient", "Stop",
        "Filter", "ForeignObject", "Graphics", "Geometry", "View", "GradientStop"]) {
        const __cn = "SVG" + __n + "Element";
        if (!g[__cn]) {
            const __C = class extends SVGElement {};
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
            // play/pause/load/canPlayType come from the HTMLMediaElement
            // surface on the Element prototype (canPlayType now reports honest
            // support for mpv-playable formats).
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
    // The text behind a Blob/File: its string parts joined (the bytes our
    // Blob keeps; non-string parts have no captured data here, so "").
    const blobText = (b) => {
        if (b === null || b === undefined) return "";
        if (typeof b === "string") return b;
        if (Array.isArray(b.__parts)) return b.__parts.map((p) => (typeof p === "string" ? p : "")).join("");
        return "";
    };
    // Text -> its UTF-8 bytes as a binary (latin1) string, so btoa() and
    // ArrayBuffer views see real bytes (not surrogate-pair chars).
    const utf8Binary = (s) => {
        const bytes = new g.TextEncoder().encode(s);
        let out = "";
        for (let i = 0; i < bytes.length; i++) out += String.fromCharCode(bytes[i]);
        return out;
    };
    // FileReader: async reads off a Blob/File. We already hold the blob's
    // bytes in JS, so the read is local; we still settle on a macrotask
    // (setTimeout 0) like the platform, firing loadstart -> load -> loadend
    // (or error) and the matching on* handlers.
    g.FileReader = class FileReader extends EventTarget {
        constructor() {
            super();
            this.readyState = 0; // EMPTY
            this.result = null;
            this.error = null;
            this.onloadstart = null; this.onprogress = null; this.onload = null;
            this.onabort = null; this.onerror = null; this.onloadend = null;
        }
        get EMPTY() { return 0; }
        get LOADING() { return 1; }
        get DONE() { return 2; }
        __fire(t) {
            const ev = new Event(t); ev.target = this; ev.currentTarget = this;
            const on = this["on" + t];
            if (typeof on === "function") { try { on.call(this, ev); } catch (e) { trust.errors.push("filereader on" + t + ": " + ((e && e.message) || e)); } }
            try { dispatch(this, ev, false); } catch (e) {}
        }
        __read(blob, makeResult) {
            this.readyState = 1; // LOADING
            this.result = null; this.error = null;
            this.__fire("loadstart");
            const self = this;
            g.setTimeout(() => {
                try {
                    const r = makeResult(blob);
                    self.result = r; self.readyState = 2;
                    self.__fire("progress"); self.__fire("load"); self.__fire("loadend");
                } catch (e) {
                    self.error = g.DOMException ? new g.DOMException(String((e && e.message) || e), "NotReadableError") : e;
                    self.readyState = 2;
                    self.__fire("error"); self.__fire("loadend");
                }
            }, 0);
        }
        readAsText(blob) { this.__read(blob, (b) => blobText(b)); }
        readAsBinaryString(blob) { this.__read(blob, (b) => utf8Binary(blobText(b))); }
        readAsDataURL(blob) {
            this.__read(blob, (b) => {
                const type = (b && b.type) || "application/octet-stream";
                return "data:" + type + ";base64," + g.btoa(utf8Binary(blobText(b)));
            });
        }
        readAsArrayBuffer(blob) {
            this.__read(blob, (b) => g.TextEncoder ? new g.TextEncoder().encode(blobText(b)).buffer : new ArrayBuffer(0));
        }
        abort() {
            if (this.readyState !== 1) return;
            this.readyState = 2; this.result = null;
            this.__fire("abort"); this.__fire("loadend");
        }
    };
    g.FileReader.EMPTY = 0; g.FileReader.LOADING = 1; g.FileReader.DONE = 2;

    g.window = g; g.self = g; g.top = g; g.parent = g;
    // `window.frames` is the WindowProxy itself in a browser (an array-like of
    // child browsing contexts). With no child frames it's just `window`, so
    // `window.frames[name]` is a plain undefined lookup rather than a throw.
    // Consent-management bootstraps (IAB TCF `__tcfapiLocator` probes —
    // FastCMP, Quantcast, every CMP stub) read `window.frames[locatorName]`
    // unguarded; a missing `frames` made that a "convert undefined to object".
    g.frames = g;
    g.DOMTokenList = DOMTokenList;
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
        baseHrefCache = null; // the base resolves against location.href
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
        // Real browsers report a region-qualified BCP-47 tag (Chrome/Firefox
        // default to "en-US"), not a bare "en". Language detectors key off
        // this: Open WebUI's i18n loads exactly the detected tag, and its
        // bundle ships "en-US" (no bare "en"), so a bare "en" missed the map
        // and rejected the translation load.
        userAgent: cfg.ua, language: "en-US", languages: ["en-US", "en"],
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
    // window.open: open a new browsing context. A single-view TUI has none, so
    // this is a no-op that returns a minimal stub window (NEVER null — page
    // code routinely chains `window.open(...).focus()`) and never throws. A
    // missing `window.open` was an UNCAUGHT TypeError that aborted click
    // handlers mid-flow (erome's age gate calls `window.open(url)` then
    // `location.href = ...`; the throw killed the navigation). We deliberately
    // don't navigate the current view for a programmatic popup — ad/popunder
    // scripts abuse it — so flows that fall back to `location.href` proceed.
    g.open = function (url) {
        const u = url === undefined ? "" : String(url);
        return {
            closed: false, name: "",
            focus() {}, blur() {}, print() {}, close() { this.closed = true; },
            postMessage() {}, moveTo() {}, resizeTo() {}, scroll() {}, scrollTo() {},
            location: { href: u, assign() {}, replace() {}, reload() {}, toString() { return u; } },
            document: g.document, opener: g,
        };
    };
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
    // --- MutationObserver (real) ---------------------------------------
    // A pure-JS DOM-mutation observer, delivered as a microtask exactly like
    // the spec's "mutation observer microtask". It does NOT challenge the
    // freeze-at-rest invariant: records are emitted ONLY by the mutation
    // wrappers below (appendChild/setAttribute/…), which run only inside a
    // compute window (load, settle, a click/form dispatch). At rest nothing
    // mutates, so nothing fires — no idle CPU, no background ticking. (Timers
    // remain frozen at rest by design; that is a separate, deliberate choice.)
    //
    // Records are recorded against the live observer list `MO`. The hot path
    // (zero observers) is a single `MO.length` check at each mutation site;
    // with observers present, an unrelated mutation costs a `target ===` identity
    // test per registration (no ancestor walk unless that registration is
    // `subtree`). The subtree match is the one Rust syscall `__dom_contains`
    // (not a JS parent walk — trap #9).
    //
    // MO and each observer's registration list are PLAIN ARRAYS, deliberately
    // NOT Boa Set/Map: a Map/Set `for…of` holds a `MapLock` whose GC finalizer
    // re-borrows the backing map, and under the heavy allocation this hot loop
    // does (a record object per mutation) a GC mid-iteration trips
    // "Object already borrowed". Arrays have no such finalizer.
    const MO = [];               // live observers (each with a per-observer record queue)
    const MO_EMPTY = Object.freeze([]); // shared empty addedNodes/removedNodes (frozen ⇒ safe to share)
    let moQueued = false;        // a delivery microtask is already scheduled
    let moChain = 0;             // consecutive delivery turns without the queue going quiet
    let moDisabled = false;      // tripped if an observer-feeds-observer loop runs away
    const MO_CHAIN_CAP = 1000;   // microtask-checkpoint lid (the spec has none; we need one)

    // Reset the loop guard at the start of each fresh compute window so a
    // pathological burst in one dispatch can't permanently mute a later one.
    function moResetGuard() { moChain = 0; moDisabled = false; }
    g.__trust.moResetGuard = moResetGuard;

    function moEnqueue() {
        if (moQueued || moDisabled) return;
        moQueued = true;
        Promise.resolve().then(moDeliver);
    }
    function moDeliver() {
        moQueued = false;
        if (moDisabled) return;
        if (++moChain > MO_CHAIN_CAP) {
            moDisabled = true;
            for (let i = 0; i < MO.length; i++) MO[i].__records = [];
            trust.errors.push("MutationObserver: delivery exceeded " + MO_CHAIN_CAP +
                " microtask turns (observer loop?) — disabled for this page");
            return;
        }
        // Snapshot the observer list: a callback may observe/disconnect mid-loop.
        const obs = MO.slice();
        for (let i = 0; i < obs.length; i++) {
            const o = obs[i];
            if (!o.__records.length) continue;
            const recs = o.__records;
            o.__records = [];
            try { o.__cb(recs, o); }
            catch (e) { trust.errors.push("MutationObserver callback: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
        }
        // The chain has ended iff no callback re-queued during this turn; only
        // then is it safe to clear the loop counter.
        if (!moQueued) moChain = 0;
    }

    function moIsAncestor(anc, node) {
        // anc strictly contains node? Direct-target matches are handled by the
        // `t === target` test; this is only consulted for subtree observers.
        return !!(node && __dom_contains(anc.__id, node.__id));
    }

    // Queue `rec` to every interested observer. `rec.type` is one of
    // "childList" | "attributes" | "characterData"; oldValue is nulled per
    // observer unless one of its matching registrations asked for it (spec).
    function moNotify(rec) {
        // 1-entry cache for the subtree ancestor test within THIS mutation:
        // multiple subtree observers commonly share a root (Steam registers 3
        // separate `#document subtree` observers), and they are scanned
        // consecutively, so the same `__dom_contains(root, target)` was run
        // once per observer. Caching the last (rootId -> result) collapses
        // those identical syscalls to one — no allocation, no semantic change.
        let cAid = null, cRes = false;
        // Deferred sibling capture: an insert/remove passes `__sib` (the node)
        // instead of pre-read `previousSibling`/`nextSibling`. We resolve them
        // ONCE, lazily, at the first matching observer — so a childList mutation
        // on a DETACHED subtree (jQuery builds offscreen; ~80% of Steam's) that
        // matches nobody pays NO sibling syscalls/wraps. Computed here it is
        // still synchronous with the mutation (insert: after `__dom_append`;
        // remove: before `__dom_detach`, since moNotify runs inside moChildRemove
        // before the detach), so the snapshot is spec-correct.
        let sibDone = false, prevSib = null, nextSib = null;
        // Resolve the record's type to booleans ONCE — moNotify runs per mutation
        // and the inner loop is per (observer × registration), so re-comparing
        // the `rec.type` string each iteration was the bulk of its cost.
        const isCL = rec.type === "childList", isAttr = rec.type === "attributes";
        for (let i = 0; i < MO.length; i++) {
            const o = MO[i], regs = o.__targets;
            let matched = false, wantOld = false;
            for (let j = 0; j < regs.length; j++) {
                const opts = regs[j];
                if (isCL ? !opts.childList : isAttr ? !opts.attributes : !opts.characterData) continue;
                let hit = opts.target === rec.target;
                if (!hit && opts.subtree) {
                    const aid = opts.target.__id;
                    if (aid === cAid) hit = cRes;
                    else { cRes = moIsAncestor(opts.target, rec.target); cAid = aid; hit = cRes; }
                }
                if (!hit) continue;
                if (isAttr && opts.attributeFilter &&
                    opts.attributeFilter.indexOf(rec.attributeName) < 0) continue;
                matched = true;
                if ((isAttr && opts.attributeOldValue) ||
                    (!isCL && !isAttr && opts.characterDataOldValue)) { wantOld = true; break; }
            }
            if (!matched) continue;
            if (rec.__sib !== undefined && !sibDone) {
                sibDone = true;
                const s = rec.__sib;
                prevSib = s ? wrap(__dom_prev(s.__id)) : null;
                nextSib = s ? wrap(__dom_next(s.__id)) : null;
            }
            o.__records.push({
                type: rec.type,
                target: rec.target,
                addedNodes: rec.addedNodes || MO_EMPTY,
                removedNodes: rec.removedNodes || MO_EMPTY,
                previousSibling: rec.__sib !== undefined ? prevSib : (rec.previousSibling || null),
                nextSibling: rec.__sib !== undefined ? nextSib : (rec.nextSibling || null),
                attributeName: rec.attributeName || null,
                attributeNamespace: null,
                oldValue: wantOld ? (rec.oldValue === undefined ? null : rec.oldValue) : null,
            });
        }
        moEnqueue();
    }

    // Emission helpers used by the mutation wrappers. Each bails on the
    // zero-observer fast path before touching the DOM for siblings/oldValue.
    function moChildInsert(parent, node) {        // call AFTER the insert
        if (!MO.length) return;
        // `__sib: node` defers prev/next-sibling capture into moNotify (resolved
        // only if some observer matches — see there). Was: eager
        // `previousSibling: node.previousSibling, …`, 2 syscalls + 2 wraps on
        // EVERY insert including the detached ones nobody observes.
        moNotify({ type: "childList", target: parent, addedNodes: [node], __sib: node });
    }
    function moChildRemove(parent, node) {        // call BEFORE the detach
        if (!MO.length) return;
        moNotify({ type: "childList", target: parent, removedNodes: [node], __sib: node });
    }
    function moChildBulk(target, removed, added) { // innerHTML / textContent / insertAdjacentHTML
        if (!MO.length) return;
        moNotify({ type: "childList", target, addedNodes: added, removedNodes: removed });
    }
    function moAttr(target, name, oldValue) {
        if (!MO.length) return;
        moNotify({ type: "attributes", target, attributeName: name, oldValue });
    }
    function moCharData(target, oldValue) {
        if (!MO.length) return;
        moNotify({ type: "characterData", target, oldValue });
    }

    g.MutationObserver = class MutationObserver {
        constructor(cb) {
            if (typeof cb !== "function")
                throw new TypeError("Failed to construct 'MutationObserver': parameter 1 is not a function");
            this.__cb = cb;
            this.__records = [];
            this.__targets = []; // array of registrations: { target, childList, … }
        }
        observe(target, options) {
            if (!target || typeof target.__id !== "number")
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': parameter 1 is not of type 'Node'");
            options = options || {};
            let attributes = options.attributes;
            let characterData = options.characterData;
            const childList = !!options.childList;
            const subtree = !!options.subtree;
            const attributeOldValue = !!options.attributeOldValue;
            const characterDataOldValue = !!options.characterDataOldValue;
            const attributeFilter = options.attributeFilter
                ? Array.prototype.map.call(options.attributeFilter, String) : null;
            // Spec defaults: an *OldValue/Filter flag implies its category.
            if (attributes === undefined) attributes = !!(attributeOldValue || attributeFilter);
            if (characterData === undefined) characterData = !!characterDataOldValue;
            if (!childList && !attributes && !characterData)
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object must set at least one of 'attributes', 'characterData', or 'childList' to true.");
            if (attributeOldValue && !attributes)
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object may only set 'attributeOldValue' to true when 'attributes' is true or not present.");
            if (attributeFilter && !attributes)
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object may only set 'attributeFilter' when 'attributes' is true or not present.");
            if (characterDataOldValue && !characterData)
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object may only set 'characterDataOldValue' to true when 'characterData' is true or not present.");
            // Re-observing the same node REPLACES its options (spec). Records
            // already queued for this observer survive (not the registration).
            const reg = { target, childList, attributes, characterData, subtree,
                attributeOldValue, characterDataOldValue, attributeFilter };
            let replaced = false;
            for (let i = 0; i < this.__targets.length; i++) {
                if (this.__targets[i].target === target) { this.__targets[i] = reg; replaced = true; break; }
            }
            if (!replaced) this.__targets.push(reg);
            if (MO.indexOf(this) < 0) MO.push(this);
        }
        disconnect() {
            this.__targets = [];
            this.__records = [];
            const i = MO.indexOf(this);
            if (i >= 0) MO.splice(i, 1);
        }
        takeRecords() { const r = this.__records; this.__records = []; return r; }
    };
    // The terminal has no live scroll and timers freeze at rest, so we REPORT
    // every observed target as FULLY intersecting, once, asynchronously — else
    // below-the-fold lazy/virtualized content (infinite scrollers, lazy tiles)
    // would never materialize, since the scroll that would reveal it can't
    // happen. We render the whole document into the scrollback, so for
    // visibility purposes the WHOLE document is the viewport: every observed
    // element is fully in view. Hence `isIntersecting:true`, `intersectionRatio:
    // 1`, and `intersectionRect == boundingClientRect`, ALL consistent — a
    // lazy-loader that gates on `intersectionRatio > 0` and ignores
    // `isIntersecting` (vanilla-lazyload does exactly this) then loads its
    // below-fold images instead of leaving them blank (humblebundle.com bundle
    // item grids). `boundingClientRect` stays the element's REAL box (a layout
    // pass backs getBoundingClientRect), so measure-then-render code downstream
    // still sees true geometry; only the intersection signal is the deliberate
    // whole-document-is-visible deviation the medium demands.
    g.__viewportRect = () => {
        const vw = g.innerWidth, vh = g.innerHeight;
        return { x: 0, y: 0, left: 0, top: 0, right: vw, bottom: vh, width: vw, height: vh };
    };
    g.IntersectionObserver = class {
        constructor(cb) { this.__cb = cb; this.__dead = false; }
        observe(el) {
            g.setTimeout(() => {
                if (this.__dead) return;
                const r = (el && el.getBoundingClientRect) ? el.getBoundingClientRect() : g.__viewportRect();
                try {
                    this.__cb([{ isIntersecting: true, intersectionRatio: 1, target: el,
                        time: 0, boundingClientRect: r, intersectionRect: r, rootBounds: g.__viewportRect() }], this);
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
                const r = (el && el.getBoundingClientRect) ? el.getBoundingClientRect() : g.__viewportRect();
                const box = [{ inlineSize: r.width, blockSize: r.height }];
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

    // MediaError — the interface a media element's `.error` exposes. Ad/video
    // SDKs (tsyndicate, players) reference the global during feature
    // detection; without it a bare `MediaError` reference throws
    // ReferenceError and aborts their init. Constants live on both the
    // constructor and the prototype, per the IDL.
    {
        const codes = {
            MEDIA_ERR_ABORTED: 1, MEDIA_ERR_NETWORK: 2,
            MEDIA_ERR_DECODE: 3, MEDIA_ERR_SRC_NOT_SUPPORTED: 4,
        };
        class MediaError {
            constructor(code, message) {
                this.code = code === undefined ? 0 : code | 0;
                this.message = message === undefined ? "" : String(message);
            }
            get [Symbol.toStringTag]() { return "MediaError"; }
        }
        for (const k in codes) { MediaError[k] = codes[k]; MediaError.prototype[k] = codes[k]; }
        g.MediaError = MediaError;
    }

    g.addEventListener = (t, f) => {
        if (typeof f === "function" || (f && typeof f.handleEvent === "function")) {
            const l = lsFor(g, String(t));
            if (!l.includes(f)) l.push(f);
        }
    };
    g.removeEventListener = (t, f) => { const l = lsFor(g, String(t)); const i = l.indexOf(f); if (i >= 0) l.splice(i, 1); };
    g.dispatchEvent = (ev) => dispatch(g, ev, false);
    // `window.postMessage(message[, targetOrigin][, transfer])` (HTML web
    // messaging). With no foreign frames the only valid target is ourselves, so
    // we deliver `message` to our own window ASYNCHRONOUSLY (a task) as a
    // `MessageEvent` carrying data/origin/source — exactly the observable spec
    // behaviour a single-window page sees. `targetOrigin`/`transfer` are
    // accepted and ignored (no cross-origin gate, nothing to transfer). A
    // structured-clone is approximated as identity. Pages post to themselves to
    // defer work or hand off across a microtask boundary (Steam's focus-restore
    // handshake posts `"FocusRestoreReady"` and listens for it); a missing
    // `window.postMessage` was an uncaught TypeError in that timer.
    g.postMessage = function (message) {
        setTimeout(function () {
            g.dispatchEvent(new MessageEvent("message", {
                data: message,
                origin: (g.location && g.location.origin) || "",
                source: g,
            }));
        }, 0);
    };
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
    // Performance + the Performance Timeline API. We keep no real timing
    // buffer, so the getEntries* trio returns empty arrays — but they MUST
    // exist: GitHub's React Router calls `performance.getEntriesByName(url,
    // "resource")` during render to detect a prefetch, and a missing method
    // throws a TypeError its top-level error boundary catches ("Unable to load
    // page"). All no-ops/empty are safe (no entry found -> the caller skips
    // the optimization).
    g.performance = {
        now: () => 0,
        timeOrigin: 0,
        timing: {}, navigation: {}, memory: {},
        mark() { return undefined; },
        measure() { return undefined; },
        clearMarks() {}, clearMeasures() {}, clearResourceTimings() {},
        setResourceTimingBufferSize() {},
        getEntries: () => [],
        getEntriesByName: () => [],
        getEntriesByType: () => [],
        addEventListener() {}, removeEventListener() {}, dispatchEvent() { return true; },
        toJSON() { return {}; },
    };
    // PerformanceObserver: observing never delivers entries (we keep no
    // buffer), but the constructor + methods must exist (libraries probe it).
    if (typeof g.PerformanceObserver === "undefined") {
        g.PerformanceObserver = class PerformanceObserver {
            constructor(cb) { this.__cb = cb; }
            observe() {}
            disconnect() {}
            takeRecords() { return []; }
        };
        g.PerformanceObserver.supportedEntryTypes = [];
    }

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
    // The HTML structured-clone algorithm: a deep copy that follows the
    // object graph (handling cycles), supported by every browser as a global.
    // Apps lean on it to snapshot state before mutating (Open WebUI's chat
    // submit clones the attachments list before sending — without this it threw
    // mid-submit, after the input was cleared, so the message silently never
    // sent). Covers the cloneable types pages actually use; throws DataCloneError
    // on functions/symbols like a real browser. No transfer support.
    g.structuredClone = function (value) {
        const seen = new Map();
        const clone = (v) => {
            const t = typeof v;
            if (v === null || (t !== "object" && t !== "function")) {
                if (t === "function" || t === "symbol") {
                    throw new DOMException("value could not be cloned", "DataCloneError");
                }
                return v;
            }
            if (t === "function") {
                throw new DOMException("value could not be cloned", "DataCloneError");
            }
            if (seen.has(v)) return seen.get(v);
            if (v instanceof Date) return new Date(v.getTime());
            if (v instanceof RegExp) return new RegExp(v.source, v.flags);
            if (typeof ArrayBuffer !== "undefined" && v instanceof ArrayBuffer) return v.slice(0);
            if (typeof ArrayBuffer !== "undefined" && ArrayBuffer.isView && ArrayBuffer.isView(v)) {
                if (typeof DataView !== "undefined" && v instanceof DataView) {
                    return new DataView(v.buffer.slice(0), v.byteOffset, v.byteLength);
                }
                return new v.constructor(v); // typed array: copies into a fresh buffer
            }
            let out;
            if (Array.isArray(v)) {
                out = [];
                seen.set(v, out);
                for (let i = 0; i < v.length; i++) out[i] = clone(v[i]);
                return out;
            }
            if (v instanceof Map) {
                out = new Map();
                seen.set(v, out);
                v.forEach((val, key) => out.set(clone(key), clone(val)));
                return out;
            }
            if (v instanceof Set) {
                out = new Set();
                seen.set(v, out);
                v.forEach((val) => out.add(clone(val)));
                return out;
            }
            out = {};
            seen.set(v, out);
            for (const k of Object.keys(v)) out[k] = clone(v[k]);
            return out;
        };
        return clone(value);
    };
    trust.tick = function (horizon) {
        // `horizon` is the look-ahead WINDOW from the current virtual time, not
        // an absolute cap. The old `t.at <= horizon` compared an absolute timer
        // deadline against a constant 1000, so once `timers.now` passed ~1000ms
        // (a few rAF frames / a couple of real-delay timers into the page's
        // life) NO positive-delay timer could ever fire again — its `at` was
        // always `now + delay > 1000`. That silently froze ALL timer-driven
        // work for the rest of the page: long interactive sessions and, most
        // visibly, a socket-streamed reply whose framework defers its leaf
        // render via `requestAnimationFrame` (Open WebUI's chat markdown
        // rendered an empty body — the each-block reconciled but the deferred
        // token render never ran). Anchoring the window to `timers.now` drains
        // due timers in rolling windows; long-delay timers still stay frozen
        // (they fall outside `now + horizon`, and `MAX_TICKS`/`DISPATCH_TICKS`
        // bound a repeating timer per settle), so "frozen at rest" holds.
        let best = null;
        const limit = timers.now + horizon;
        for (const t of timers.q) if (t.at <= limit && (!best || t.at < best.at)) best = t;
        if (!best) return false;
        timers.q.splice(timers.q.indexOf(best), 1);
        timers.now = Math.max(timers.now, best.at);
        if (best.every !== null) timers.q.push({ id: best.id, at: timers.now + best.every, fn: best.fn, every: best.every });
        try { best.fn(); } catch (e) { trust.errors.push("timer: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
        return true;
    };
    // The clocks advance with VIRTUAL time, not the wall clock. A click that
    // triggers an animation (jQuery's `.slideUp`/`.fadeOut`, used by Humble's
    // "Dismiss banner") drives its frames off `Date.now()`/`performance.now()`;
    // our dispatch settle advances `timers.now` 1000ms a tick, but the real
    // wall clock barely moves in that microsecond burst, so a wall-clock
    // `Date.now()` made the animation see ~0 elapsed and never finish (the
    // banner never slid away). Anchoring the clocks to the virtual timer base
    // lets click-driven animations run to completion within the dispatch —
    // consistent with timers being frozen at rest and advancing only on
    // interaction. `Date.now()` keeps a realistic absolute base so timestamps
    // still look real; `performance.now()` is the elapsed virtual time.
    const __epoch0 = Date.now();
    Date.now = () => __epoch0 + timers.now;
    g.performance.now = () => timers.now;

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
    // --- WHATWG Streams (in-memory). Real constructors so streaming code
    // both LOADS and RUNS: Open WebUI's chat/SSE pipeline does
    // `class X extends TransformStream` at module-eval time and pipes
    // `body.pipeThrough(new TextDecoderStream()).pipeThrough(parser)` — a
    // missing `TransformStream` threw a ReferenceError that 500'd the whole
    // route. Single-consumer, no BYOB/byte-stream/backpressure tuning;
    // faithful enough for producer→transform→reader pipelines. (A fetch
    // Response.body is still null — actual network streaming is a separate,
    // deeper feature — so the streams a page builds itself work, but reading a
    // response AS a stream doesn't yet.) ---
    {
        const deferred = () => {
            let resolve, reject;
            const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
            return { promise, resolve, reject };
        };
        class ReadableStream {
            constructor(source, strategy) {
                source = source || {};
                this._source = source;
                this._queue = [];
                this._pending = []; // waiting read()s: {resolve,reject}
                this._closed = false;
                this._error = null;
                this._reader = null;
                this._closedDef = deferred();
                this._closedDef.promise.catch(() => {});
                const self = this;
                this._controller = {
                    enqueue(chunk) {
                        if (self._closed || self._error) return;
                        if (self._pending.length) self._pending.shift().resolve({ value: chunk, done: false });
                        else self._queue.push(chunk);
                    },
                    close() {
                        if (self._closed || self._error) return;
                        self._closed = true;
                        while (self._pending.length) self._pending.shift().resolve({ value: undefined, done: true });
                        self._closedDef.resolve(undefined);
                    },
                    error(e) {
                        if (self._closed || self._error) return;
                        self._error = e || new TypeError("stream error");
                        while (self._pending.length) self._pending.shift().reject(self._error);
                        self._closedDef.reject(self._error);
                    },
                    get desiredSize() { return self._closed || self._error ? null : 1; },
                };
                try {
                    const r = source.start ? source.start(this._controller) : undefined;
                    Promise.resolve(r).then(() => self._pull(), (e) => self._controller.error(e));
                } catch (e) { this._controller.error(e); }
            }
            _pull() {
                const self = this;
                if (this._closed || this._error || !this._source.pull) return;
                if (this._pending.length && this._queue.length === 0) {
                    try { Promise.resolve(this._source.pull(this._controller)).catch((e) => self._controller.error(e)); }
                    catch (e) { self._controller.error(e); }
                }
            }
            get locked() { return this._reader !== null; }
            getReader(opts) {
                if (this._reader) throw new TypeError("ReadableStream is already locked");
                const self = this;
                const reader = {
                    read() {
                        if (self._queue.length) return Promise.resolve({ value: self._queue.shift(), done: false });
                        if (self._error) return Promise.reject(self._error);
                        if (self._closed) return Promise.resolve({ value: undefined, done: true });
                        return new Promise((resolve, reject) => { self._pending.push({ resolve, reject }); self._pull(); });
                    },
                    cancel(reason) { return self.cancel(reason); },
                    releaseLock() { self._reader = null; },
                    get closed() { return self._closedDef.promise; },
                };
                this._reader = reader;
                return reader;
            }
            cancel(reason) {
                if (!this._closed && !this._error) {
                    this._closed = true;
                    this._queue = [];
                    while (this._pending.length) this._pending.shift().resolve({ value: undefined, done: true });
                    this._closedDef.resolve(undefined);
                    try { if (this._source.cancel) this._source.cancel(reason); } catch (e) {}
                }
                return Promise.resolve(undefined);
            }
            pipeTo(dest, opts) {
                const reader = this.getReader();
                const writer = dest.getWriter();
                return new Promise((resolve, reject) => {
                    const step = () => {
                        reader.read().then((res) => {
                            if (res.done) { Promise.resolve(writer.close()).then(resolve, resolve); return; }
                            Promise.resolve(writer.write(res.value)).then(step, reject);
                        }, reject);
                    };
                    step();
                });
            }
            pipeThrough(pair, opts) {
                this.pipeTo(pair.writable, opts);
                return pair.readable;
            }
            tee() {
                const reader = this.getReader();
                let c1 = null, c2 = null, reading = false;
                const pump = () => {
                    if (reading) return;
                    reading = true;
                    reader.read().then((res) => {
                        reading = false;
                        if (res.done) { if (c1) c1.close(); if (c2) c2.close(); return; }
                        if (c1) c1.enqueue(res.value);
                        if (c2) c2.enqueue(res.value);
                    }, (e) => { if (c1) c1.error(e); if (c2) c2.error(e); });
                };
                const b1 = new ReadableStream({ start(c) { c1 = c; }, pull: pump });
                const b2 = new ReadableStream({ start(c) { c2 = c; }, pull: pump });
                return [b1, b2];
            }
        }
        ReadableStream.prototype[Symbol.asyncIterator] = function () {
            const reader = this.getReader();
            return {
                next() { return reader.read(); },
                return() { reader.releaseLock(); return Promise.resolve({ value: undefined, done: true }); },
            };
        };
        class WritableStream {
            constructor(sink, strategy) {
                sink = sink || {};
                this._sink = sink;
                this._writer = null;
                this._closed = false;
                this._error = null;
                const self = this;
                this._controller = { error(e) { self._error = e; }, get signal() { return undefined; } };
                try { this._ready = Promise.resolve(sink.start ? sink.start(this._controller) : undefined); }
                catch (e) { this._ready = Promise.reject(e); }
                this._chain = this._ready.catch(() => {});
            }
            get locked() { return this._writer !== null; }
            getWriter() {
                if (this._writer) throw new TypeError("WritableStream is already locked");
                const self = this;
                const writer = {
                    write(chunk) {
                        self._chain = self._chain.then(() => {
                            if (self._error) throw self._error;
                            return self._sink.write ? self._sink.write(chunk, self._controller) : undefined;
                        });
                        return self._chain;
                    },
                    close() {
                        self._chain = self._chain.then(() => {
                            if (self._closed) return undefined;
                            self._closed = true;
                            return self._sink.close ? self._sink.close() : undefined;
                        });
                        return self._chain;
                    },
                    abort(reason) {
                        self._error = reason || new TypeError("aborted");
                        return Promise.resolve(self._sink.abort ? self._sink.abort(reason) : undefined);
                    },
                    releaseLock() { self._writer = null; },
                    get ready() { return self._ready.then(() => undefined); },
                    get closed() { return self._chain.then(() => undefined); },
                    get desiredSize() { return self._error ? null : (self._closed ? 0 : 1); },
                };
                this._writer = writer;
                return writer;
            }
            abort(reason) { this._error = reason; return Promise.resolve(this._sink.abort ? this._sink.abort(reason) : undefined); }
            close() { return this.getWriter().close(); }
        }
        class TransformStream {
            constructor(transformer, writableStrategy, readableStrategy) {
                transformer = transformer || {};
                let rc;
                this.readable = new ReadableStream({ start(c) { rc = c; } });
                const tc = {
                    enqueue(chunk) { rc.enqueue(chunk); },
                    terminate() { rc.close(); },
                    error(e) { rc.error(e); },
                    get desiredSize() { return rc.desiredSize; },
                };
                let started;
                try { started = transformer.start ? transformer.start(tc) : undefined; }
                catch (e) { rc.error(e); started = Promise.reject(e); }
                this.writable = new WritableStream({
                    start() { return Promise.resolve(started); },
                    write(chunk) {
                        if (transformer.transform) return Promise.resolve(transformer.transform(chunk, tc));
                        tc.enqueue(chunk);
                        return undefined;
                    },
                    close() {
                        return Promise.resolve(transformer.flush ? transformer.flush(tc) : undefined).then(() => rc.close());
                    },
                    abort(e) { rc.error(e); },
                });
            }
        }
        class TextDecoderStream extends TransformStream {
            constructor(label, options) {
                const dec = new g.TextDecoder(label, options);
                super({
                    transform(chunk, c) { const s = dec.decode(chunk); if (s) c.enqueue(s); },
                });
                this._encoding = dec.encoding;
            }
            get encoding() { return this._encoding; }
        }
        class TextEncoderStream extends TransformStream {
            constructor() {
                const enc = new g.TextEncoder();
                super({ transform(chunk, c) { c.enqueue(enc.encode(String(chunk))); } });
            }
            get encoding() { return "utf-8"; }
        }
        g.ReadableStream = ReadableStream;
        g.WritableStream = WritableStream;
        g.TransformStream = TransformStream;
        g.TextDecoderStream = TextDecoderStream;
        g.TextEncoderStream = TextEncoderStream;
    }
    // --- Web Animations API (Element.animate). A terminal has no real
    // animation, so an Animation settles to "finished" immediately — on a
    // MACROTASK, so a caller assigning `onfinish` AFTER animate() (Svelte 5's
    // transition system does exactly this) still sees it fire. Critical, not
    // cosmetic: `element.animate(...)` being undefined threw inside Svelte 5's
    // intro-transition effect, and a thrown effect ABORTS the whole effect-
    // flush batch — so a sibling effect in the same flush (a TipTap/ProseMirror
    // editor's mount) silently never ran, leaving Open WebUI's chat input
    // unrendered. `finished` always resolves exactly once (finish/cancel/auto),
    // so nothing awaiting it hangs or reports an unhandled rejection. ---
    {
        class Animation extends EventTarget {
            constructor(effect, timeline) {
                super();
                this.effect = effect || null;
                this.timeline = timeline || null;
                this.id = "";
                this.playbackRate = 1;
                this.startTime = 0;
                this.currentTime = 0;
                this.pending = false;
                this.playState = "running";
                this.replaceState = "active";
                this.onfinish = null;
                this.oncancel = null;
                this.onremove = null;
                this._done = false;
                let res;
                this.finished = new Promise((r) => { res = r; });
                const self = this;
                this._settle = (kind) => {
                    if (self._done) return;
                    self._done = true;
                    self.playState = kind === "cancel" ? "idle" : "finished";
                    const ev = new Event(kind === "cancel" ? "cancel" : "finish");
                    const cb = kind === "cancel" ? self.oncancel : self.onfinish;
                    if (typeof cb === "function") { try { cb.call(self, ev); } catch (e) {} }
                    self.dispatchEvent(ev);
                    res(self);
                };
                setTimeout(() => this._settle("finish"), 0);
            }
            play() {}
            pause() { this.playState = "paused"; }
            reverse() {}
            finish() { this._settle("finish"); }
            cancel() { this._settle("cancel"); }
            updatePlaybackRate(r) { this.playbackRate = r; }
            persist() {}
            commitStyles() {}
        }
        g.Animation = Animation;
        Element.prototype.animate = function (keyframes, options) { return new Animation(null, null); };
        Element.prototype.getAnimations = function () { return []; };
        if (g.document) {
            g.document.getAnimations = function () { return []; };
            try { g.document.timeline = { currentTime: 0 }; } catch (e) {}
        }
    }
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
        // Intl.NumberFormat/DateTimeFormat/Collator are specced callable
        // WITHOUT `new` (legacy web-compat — they construct an instance
        // either way); only the newer ctors require `new`. ES classes throw
        // when called as functions, so wrap the three legacy ones in a
        // function that forwards to `new`, preserving prototype/instanceof
        // and the static supportedLocalesOf. Humble Bundle does
        // `Intl.NumberFormat(locale, opts).format(amount)` (no `new`).
        const callable = (Cls) => {
            const F = function (locales, options) { return new Cls(locales, options); };
            F.prototype = Cls.prototype;
            F.prototype.constructor = F;
            F.supportedLocalesOf = Cls.supportedLocalesOf;
            return F;
        };
        g.Intl = {
            NumberFormat: callable(NumberFormat),
            DateTimeFormat: callable(DateTimeFormat),
            Collator: callable(Collator),
            DisplayNames, PluralRules, RelativeTimeFormat,
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
    // The wire body for a request/response: the platform accepts strings,
    // URLSearchParams, Blob/File, and ArrayBuffer views; our syscall takes a
    // string, so flatten to one. Unknown objects stringify (no multipart —
    // FormData is deliberately unsupported), null stays null.
    const __bodyText = (body) => {
        if (body === null || body === undefined) return null;
        if (typeof body === "string") return body;
        if (body instanceof URLSearchParams) return body.toString();
        if (Array.isArray(body.__parts)) return blobText(body); // Blob/File
        if (typeof body.byteLength === "number") {
            try {
                const v = body instanceof ArrayBuffer ? new Uint8Array(body)
                    : new Uint8Array(body.buffer || body);
                let s = ""; for (let i = 0; i < v.length; i++) s += String.fromCharCode(v[i]);
                return s;
            } catch (e) { return ""; }
        }
        return String(body);
    };
    // Body mixin shared by Request and Response: the consumption methods read
    // the captured body once (lenient — we don't throw on re-read, unlike the
    // spec, to avoid breaking defensive double-reads).
    const __bodyMethods = {
        text() { this.bodyUsed = true; return Promise.resolve(__bodyText(this.__body) || ""); },
        json() { this.bodyUsed = true; try { return Promise.resolve(JSON.parse(__bodyText(this.__body) || "")); } catch (e) { return Promise.reject(e); } },
        arrayBuffer() {
            this.bodyUsed = true;
            const bin = utf8Binary(__bodyText(this.__body) || "");
            const buf = new ArrayBuffer(bin.length); const view = new Uint8Array(buf);
            for (let i = 0; i < bin.length; i++) view[i] = bin.charCodeAt(i) & 0xff;
            return Promise.resolve(buf);
        },
        blob() {
            this.bodyUsed = true;
            const t = (this.headers && this.headers.get && this.headers.get("content-type")) || "";
            return Promise.resolve(new g.Blob([__bodyText(this.__body) || ""], { type: t || "" }));
        },
        formData() { return Promise.reject(new TypeError("formData unsupported")); },
    };
    // Fetch API Request (https://fetch.spec.whatwg.org/#request-class). The bare
    // `Request` global is referenced by many bundles (GitHub's react-core throws
    // a ReferenceError without it).
    class Request {
        constructor(input, init) {
            init = init || {};
            const fromReq = input instanceof Request;
            this.url = fromReq ? input.url
                : String((input && input.url !== undefined) ? input.url : input);
            this.method = String(init.method || (fromReq ? input.method : null) || "GET").toUpperCase();
            this.headers = new Headers(init.headers !== undefined ? init.headers : (fromReq ? input.headers : undefined));
            this.__body = init.body !== undefined ? init.body : (fromReq ? input.__body : null);
            this.credentials = init.credentials || (fromReq ? input.credentials : "same-origin");
            this.mode = init.mode || (fromReq ? input.mode : "cors");
            this.cache = init.cache || (fromReq ? input.cache : "default");
            this.redirect = init.redirect || (fromReq ? input.redirect : "follow");
            this.referrer = init.referrer !== undefined ? init.referrer : (fromReq ? input.referrer : "about:client");
            this.referrerPolicy = init.referrerPolicy || (fromReq ? input.referrerPolicy : "");
            this.integrity = init.integrity || (fromReq ? input.integrity : "");
            this.keepalive = init.keepalive !== undefined ? !!init.keepalive : (fromReq ? input.keepalive : false);
            this.signal = init.signal || (fromReq ? input.signal : null);
            this.destination = "";
            this.bodyUsed = false;
            this.body = null; // no ReadableStream
        }
        clone() { return new Request(this); }
    }
    Object.assign(Request.prototype, __bodyMethods);
    g.Request = Request;
    // Fetch API Response (https://fetch.spec.whatwg.org/#response-class).
    class Response {
        constructor(body, init) {
            init = init || {};
            this.__body = body !== undefined ? body : null;
            this.status = init.status !== undefined ? (init.status | 0) : 200;
            this.statusText = init.statusText !== undefined ? String(init.statusText) : "";
            this.headers = new Headers(init.headers);
            this.ok = this.status >= 200 && this.status < 300;
            this.url = init.url ? String(init.url) : "";
            this.redirected = false;
            this.type = "default";
            this.bodyUsed = false;
            this.__bodyStream = undefined;
        }
        // The response body as a ReadableStream (lazy + cached). Streaming
        // consumers read `response.body.getReader()` — Open WebUI reads chat
        // completions (SSE) exactly this way; a null body made `getReader()`
        // throw, so the assistant reply never read back. Our network layer
        // buffers the whole body, so the stream yields it as one UTF-8 chunk then
        // closes; an SSE parser splits it identically. null only for empty bodies.
        get body() {
            if (this.__bodyStream !== undefined) return this.__bodyStream;
            if (this.__body === null || this.__body === undefined) { this.__bodyStream = null; return null; }
            const bytes = new g.TextEncoder().encode(__bodyText(this.__body) || "");
            this.__bodyStream = new g.ReadableStream({
                start(c) { if (bytes.length) c.enqueue(bytes); c.close(); },
            });
            return this.__bodyStream;
        }
        clone() {
            const r = new Response(this.__body, { status: this.status, statusText: this.statusText, headers: this.headers, url: this.url });
            r.type = this.type; r.redirected = this.redirected; return r;
        }
        static error() { const r = new Response(null, { status: 0 }); r.type = "error"; return r; }
        static redirect(url, status) { const r = new Response(null, { status: status || 302 }); r.headers.set("location", String(url)); return r; }
        static json(data, init) { const r = new Response(JSON.stringify(data), init); if (!r.headers.has("content-type")) r.headers.set("content-type", "application/json"); return r; }
    }
    Object.assign(Response.prototype, __bodyMethods);
    g.Response = Response;
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

    // BroadcastChannel: same-origin cross-context messaging. A terminal
    // browser has one page (no other tabs/workers), so the only peers are
    // other channels of the same name in THIS page — we deliver to them
    // (excluding the sender, per spec) on a macrotask, and a lone channel
    // simply never receives, exactly as a single tab would. SvelteKit opens
    // one at boot for session sync; a missing global was a ReferenceError
    // that aborted the whole app mount. `BC` maps name→array of live
    // channels (an array, never iterated as a Boa Map — see the MO trap).
    const BC = new Map();
    class BroadcastChannel extends EventTarget {
        constructor(name) {
            super();
            this.name = String(name);
            this.onmessage = null; this.onmessageerror = null;
            this.__closed = false;
            let list = BC.get(this.name);
            if (!list) { list = []; BC.set(this.name, list); }
            list.push(this);
        }
        postMessage(message) {
            if (this.__closed) throw new DOMException("channel is closed", "InvalidStateError");
            const list = BC.get(this.name) || [];
            for (const ch of list.slice()) {
                if (ch === this || ch.__closed) continue;
                g.setTimeout(() => {
                    if (ch.__closed) return;
                    const ev = new MessageEvent("message", {
                        data: message,
                        origin: (g.location && g.location.origin) || "",
                    });
                    if (typeof ch.onmessage === "function") { try { ch.onmessage.call(ch, ev); } catch (e) {} }
                    ch.dispatchEvent(ev);
                }, 0);
            }
        }
        close() {
            this.__closed = true;
            const list = BC.get(this.name);
            if (list) { const i = list.indexOf(this); if (i >= 0) list.splice(i, 1); }
        }
    }
    g.BroadcastChannel = BroadcastChannel;

    // --- WebSocket (RFC 6455 transport in ws.rs; socket.io rides it) ---
    // A real connection: `__ws_open` spawns the Rust task, inbound frames arrive
    // as `__trust.wsEvent` calls (the actor dispatches them like clicks). This is
    // what lets a websocket-enabled app (Open WebUI) stream chat tokens back —
    // the page's own socket.io-client runs the protocol over these frames.
    const WS_REGISTRY = {};
    class WebSocket extends EventTarget {
        constructor(url, protocols) {
            super();
            this.url = String(url);
            this.readyState = 0; // CONNECTING
            this.bufferedAmount = 0;
            this.extensions = "";
            this.protocol = "";
            this.binaryType = "blob";
            this.onopen = null; this.onmessage = null; this.onclose = null; this.onerror = null;
            let proto = "";
            if (Array.isArray(protocols)) proto = protocols.join(",");
            else if (protocols !== undefined && protocols !== null) proto = String(protocols);
            this.__id = __ws_open(this.url, proto);
            if (this.__id < 0) {
                // Synchronous open failure (bad URL / blocked / no net grant):
                // a browser still reports it asynchronously as error + close.
                const self = this;
                g.setTimeout(() => {
                    self.readyState = 3;
                    self.__fire("error", {});
                    self.__fire("close", { code: 1006, reason: "", wasClean: false });
                }, 0);
            } else {
                WS_REGISTRY[this.__id] = this;
            }
        }
        get CONNECTING() { return 0; } get OPEN() { return 1; }
        get CLOSING() { return 2; } get CLOSED() { return 3; }
        send(data) {
            if (this.readyState === 0) throw new DOMException("WebSocket is still CONNECTING", "InvalidStateError");
            if (this.readyState !== 1) return;
            if (typeof data === "string") { __ws_send(this.__id, data, false); return; }
            let bytes = null;
            if (data instanceof ArrayBuffer) bytes = new Uint8Array(data);
            else if (data && data.buffer instanceof ArrayBuffer) bytes = new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength);
            if (bytes) {
                let s = ""; for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
                __ws_send(this.__id, s, true);
            } else {
                __ws_send(this.__id, String(data), false); // Blob/other: best-effort
            }
        }
        close(code, reason) {
            if (this.readyState >= 2) return;
            this.readyState = 2; // CLOSING
            __ws_close(this.__id, (code === undefined || code === null) ? 1000 : (code | 0), reason ? String(reason) : "");
        }
        __fire(type, init) {
            let ev;
            if (type === "message") ev = new MessageEvent("message", init);
            else if (type === "close") ev = new CloseEvent("close", init);
            else ev = new Event(type);
            const h = this["on" + type];
            if (typeof h === "function") { try { h.call(this, ev); } catch (e) { trust.errors.push("ws on" + type + ": " + ((e && e.message) || e)); } }
            this.dispatchEvent(ev);
        }
    }
    WebSocket.CONNECTING = 0; WebSocket.OPEN = 1; WebSocket.CLOSING = 2; WebSocket.CLOSED = 3;
    g.WebSocket = WebSocket;
    // The actor calls this for every inbound WebSocket event (open/message/close).
    trust.wsEvent = function (id, kind, data, isBinary, code, reason) {
        const ws = WS_REGISTRY[id];
        if (!ws) return;
        if (kind === "open") {
            ws.readyState = 1; // OPEN
            ws.__fire("open", {});
        } else if (kind === "message") {
            let payload = data;
            if (isBinary) {
                const len = data.length, buf = new ArrayBuffer(len), view = new Uint8Array(buf);
                for (let i = 0; i < len; i++) view[i] = data.charCodeAt(i) & 0xFF;
                payload = (ws.binaryType === "arraybuffer") ? buf : new g.Blob([buf]);
            }
            ws.__fire("message", { data: payload, origin: ws.url });
        } else if (kind === "close") {
            ws.readyState = 3; // CLOSED
            delete WS_REGISTRY[id];
            ws.__fire("close", { code: code, reason: reason || "", wasClean: code === 1000 });
        }
    };

    // Flatten a header map ({lowercased-name: value}) into the `k\nv\nk\nv`
    // blob the `__http_fetch` syscalls forward to the request. Lets a page's
    // `setRequestHeader`/`init.headers` (X-Requested-With, Authorization, …)
    // actually reach the wire instead of being dropped.
    function __hdrBlob(h) {
        let s = "";
        for (const k in h) {
            if (!Object.prototype.hasOwnProperty.call(h, k)) continue;
            s += (s ? "\n" : "") + k + "\n" + h[k];
        }
        return s;
    }
    g.fetch = function (input, init) {
        try {
            // Normalize input+init into a Request (input may be a URL string,
            // a Request, or a URL object).
            const req = new Request(input, init);
            const url = req.url;
            const body = __bodyText(req.__body);
            const ctype = req.headers.get("content-type")
                || (body !== null ? "text/plain;charset=UTF-8" : null);
            return __http_fetch_async(url, req.method, body, ctype, __hdrBlob(req.headers.__h)).then(function (r) {
                if (!r) throw new TypeError("fetch failed or blocked: " + url);
                const status = r[0], respCType = r[1], text = r[2];
                const hdrs = {}; if (respCType) hdrs["content-type"] = respCType;
                const resp = new Response(text, { status: status, statusText: "", headers: hdrs, url: url });
                resp.type = "basic";
                return resp;
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
            const hdrs = __hdrBlob(this.__h);
            if (this.__sync) {
                this.__finish(__http_fetch(this.__url, this.__method || "GET", b, ctype, hdrs));
            } else {
                // The request runs concurrently, but its callbacks are
                // macrotasks (not microtasks): defer __finish into the
                // timer queue so promise reactions still run first, as on
                // the real platform.
                const xhr = this;
                __http_fetch_async(this.__url, this.__method || "GET", b, ctype, hdrs)
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

        // (e) the CACHED path (Step 4 prelude compile cache): compile the image
        // ONCE, then per page rehydrate it into an empty carrier script and run —
        // exactly what `run_prelude` does on every page after the first. The A/B
        // against (b)/(c). `rehydrate` isolates `from_image` (the work that
        // replaces parse+compile); `cached_total` is the whole per-page cost.
        let image = {
            let mut c = build_ctx();
            let script =
                boa_engine::Script::parse(Source::from_bytes(PRELUDE.as_bytes()), None, &mut c)
                    .unwrap();
            script.codeblock(&mut c).unwrap().to_image()
        };
        let (mut rehydrate_acc, mut cached_total_acc) = (Duration::ZERO, Duration::ZERO);
        for _ in 0..N {
            let mut c = build_ctx();
            let t = Instant::now();
            let shell =
                boa_engine::Script::parse(Source::from_bytes("".as_bytes()), None, &mut c).unwrap();
            let t_re = Instant::now();
            shell.set_codeblock(boa_engine::vm::CodeBlock::from_image(&image));
            rehydrate_acc += t_re.elapsed();
            shell.evaluate(&mut c).unwrap();
            cached_total_acc += t.elapsed();
        }
        let cached_total = cached_total_acc / N;

        eprintln!("--- PRELUDE cost (avg of {N}, release recommended) ---");
        eprintln!("PRELUDE size:                      {} bytes", PRELUDE.len());
        eprintln!("context build (+syscalls+config):  {ctx_build:?}");
        eprintln!("PRELUDE total (parse+compile+run): {prelude_total:?}");
        eprintln!("  - parse (Script::parse):         {:?}", parse_acc / N);
        eprintln!("  - compile+run (evaluate):        {:?}", eval_acc / N);
        eprintln!("CACHED total (rehydrate+run):      {cached_total:?}");
        eprintln!("  - rehydrate (from_image):        {:?}", rehydrate_acc / N);
        eprintln!(
            "saved per cached page:             {:?}",
            prelude_total.saturating_sub(cached_total)
        );
        eprintln!("tiny post-prelude page call:       {tiny_call:?}");
    }

    /// The cross-page CDN compile cache win (Phase 2): for each real classic CDN
    /// library, the COLD per-page cost (parse + compile, what a non-cached page
    /// pays) vs the CACHED per-page cost (rehydrate the detached image). The
    /// saving is what every page after the first in a session avoids. Run:
    /// `cargo test --release cdn_cache_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "manual measurement; needs the canary bundles in target/canary/"]
    fn cdn_cache_cost() {
        const N: u32 = 30;
        eprintln!("--- CDN cache cost (avg of {N}, release recommended) ---");
        for name in [
            "jquery-3.7.1.min.js",
            "d3.v7.min.js",
            "vue.global.prod.js",
            "react.production.min.js",
        ] {
            let Ok(src) = std::fs::read(format!("target/canary/{name}")) else {
                eprintln!("{name}: SKIP (not present)");
                continue;
            };
            // COLD: parse + compile in a fresh realm each time.
            let (mut parse_acc, mut compile_acc) = (Duration::ZERO, Duration::ZERO);
            for _ in 0..N {
                let mut c = Context::default();
                let t = Instant::now();
                let script =
                    boa_engine::Script::parse(Source::from_bytes(&src), None, &mut c).unwrap();
                parse_acc += t.elapsed();
                let t = Instant::now();
                script.codeblock(&mut c).unwrap();
                compile_acc += t.elapsed();
            }
            let cold = (parse_acc + compile_acc) / N;
            // The detached image, built once (the per-session one-time cost).
            let image = {
                let mut c = Context::default();
                let script =
                    boa_engine::Script::parse(Source::from_bytes(&src), None, &mut c).unwrap();
                script.codeblock(&mut c).unwrap().to_image()
            };
            // CACHED: rehydrate into a fresh realm each time (replaces parse+compile).
            let mut rehydrate_acc = Duration::ZERO;
            for _ in 0..N {
                let mut c = Context::default();
                let t = Instant::now();
                let shell =
                    boa_engine::Script::parse(Source::from_bytes("".as_bytes()), None, &mut c)
                        .unwrap();
                shell.set_codeblock(boa_engine::vm::CodeBlock::from_image(&image));
                rehydrate_acc += t.elapsed();
            }
            let cached = rehydrate_acc / N;
            eprintln!(
                "{name} ({} KB): cold parse {:?} + compile {:?} = {cold:?} | cached {cached:?} | saved {:?}/page",
                src.len() / 1024,
                parse_acc / N,
                compile_acc / N,
                cold.saturating_sub(cached),
            );
        }
    }

    /// What does the MutationObserver path actually COST on this engine?
    /// "Reaction time" has three parts: recording overhead per mutation (with
    /// vs without observers, plus the extra a body-rooted `subtree` observer
    /// adds via `__dom_contains`); batched delivery cost (the microtask that
    /// invokes callbacks); and the lone-mutation round trip (one change → its
    /// callback, NOT amortized — the closest thing to a single interaction's
    /// reaction latency). Run (release recommended; debug is ~10-20x slower):
    /// `cargo test --release mutation_observer_bench -- --ignored --nocapture`.
    #[test]
    #[ignore = "manual measurement"]
    fn mutation_observer_bench() {
        fn build() -> Context {
            let html = r#"<html><head></head><body><div id="root"></div></body></html>"#;
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
            ctx.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();
            ctx
        }

        const M: u32 = 5000;
        eprintln!("--- MutationObserver cost (M={M} appendChild; use --release) ---");
        // setup script -> ()  (registers the observers under test)
        let scenarios: &[(&str, &str)] = &[
            ("no observers (raw appendChild)", ""),
            (
                "1 direct observer (childList)",
                "new MutationObserver(function(){}).observe(root,{childList:true});",
            ),
            (
                "1 subtree observer on <body>",
                "new MutationObserver(function(){}).observe(document.body,{childList:true,subtree:true});",
            ),
            (
                "8 subtree observers on <body>",
                "for(var k=0;k<8;k++)new MutationObserver(function(){}).observe(document.body,{childList:true,subtree:true});",
            ),
        ];
        for (label, setup) in scenarios {
            let mut ctx = build();
            ctx.eval(Source::from_bytes(
                format!("var root=document.getElementById('root');{setup}").as_bytes(),
            ))
            .unwrap();
            let loop_src = format!(
                "for(var i=0;i<{M};i++){{root.appendChild(document.createElement('span'));}}"
            );
            let t = Instant::now();
            ctx.eval(Source::from_bytes(loop_src.as_bytes())).unwrap();
            let mutate = t.elapsed();
            let t = Instant::now();
            ctx.run_jobs().unwrap(); // the batched MutationObserver delivery runs here
            let deliver = t.elapsed();
            eprintln!(
                "{label:34} mutate {mutate:>10.3?} ({:>6.0} ns/op)  deliver {deliver:>10.3?}",
                mutate.as_nanos() as f64 / M as f64
            );
        }

        // Lone mutation -> callback round trip: a single change, then drain its
        // delivery, repeated. Includes per-turn microtask scheduling that a big
        // batch amortizes away — the honest single-interaction reaction time.
        {
            let mut ctx = build();
            ctx.eval(Source::from_bytes(
                b"var root=document.getElementById('root');globalThis.__hits=0;\
                  new MutationObserver(function(){globalThis.__hits++;}).observe(root,{childList:true});",
            ))
            .unwrap();
            const R: u32 = 2000;
            let t = Instant::now();
            for _ in 0..R {
                ctx.eval(Source::from_bytes(
                    b"root.appendChild(document.createElement('i'));",
                ))
                .unwrap();
                ctx.run_jobs().unwrap();
            }
            let rt = t.elapsed() / R;
            let hits = ctx
                .eval(Source::from_bytes(b"globalThis.__hits"))
                .unwrap()
                .to_number(&mut ctx)
                .unwrap();
            eprintln!("lone mutation -> callback round trip: {rt:?}  (deliveries={hits})");
        }
    }

    /// Engine profiler (Step 0 harness): load an arbitrary JS bundle into a
    /// faithful page context (DOM + syscalls + config + PRELUDE, exactly as
    /// `load_page` builds it) and split its cost into parse / compile /
    /// execute, plus GC stats (vendored boa_gc instrumentation). The bundle
    /// runs as a classic top-level script. Heavy real bundles (the YouTube
    /// kevlar base) won't fully boot without their page environment, but they
    /// still exercise parse+compile of the whole file and a large slab of
    /// execution — enough to find where the engine spends its seconds.
    ///
    /// Hardened for reproducibility: it repeats `TRUST_JS_BENCH_RUNS` times
    /// (default 5), DISCARDS the first (cold) run, and reports median + min/max
    /// (dispersion) per phase, plus a metadata header (commit / allocator /
    /// arch / input hash / peak RSS) so a number is reproducible next session.
    /// Each run rebuilds a fresh context and force-collects first, so the
    /// samples are independent. NB: this measures ONE bundle in isolation — it
    /// CANNOT see a live SPA's settle-time execution; for that, load the real
    /// page under `TRUST_JS_PHASE=1` (the whole-load phase report). See the JS
    /// engine performance plan, Step 0/1.
    ///   TRUST_JS_BENCH=/tmp/kevlar.js [TRUST_JS_BENCH_RUNS=5] \
    ///     cargo test --release engine_profile -- --ignored --nocapture
    /// (run under `/usr/bin/time -v` for User CPU / max RSS too).
    #[test]
    #[ignore = "manual measurement, needs TRUST_JS_BENCH=<file>"]
    fn engine_profile() {
        let Ok(path) = std::env::var("TRUST_JS_BENCH") else {
            eprintln!("set TRUST_JS_BENCH to a .js file");
            return;
        };
        let src = std::fs::read(&path).unwrap();
        let runs: usize = std::env::var("TRUST_JS_BENCH_RUNS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5)
            .max(1);
        let no_opt = std::env::var_os("TRUST_NO_OPT").is_some();

        // --- reproducibility metadata header ---
        use sha2::Digest as _;
        let hash = sha2::Sha256::digest(&src);
        let hash_hex: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
        let commit = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| String::from("unknown"));
        let alloc = if cfg!(feature = "mimalloc") {
            "mimalloc"
        } else {
            "system"
        };
        eprintln!("=== engine profile ===");
        eprintln!(
            "input    : {path}  ({} bytes, sha256:{hash_hex}…)",
            src.len()
        );
        eprintln!(
            "build    : commit {commit}  arch {}  alloc {alloc}  opt {}",
            std::env::consts::ARCH,
            if no_opt { "OFF" } else { "on" },
        );
        eprintln!("runs     : {runs} (first discarded as cold)");

        // One independent measurement: fresh context + clean heap, then time
        // parse / compile / execute of the bench source (NOT the prelude setup).
        let run_once = || -> (Duration, Duration, Duration, (usize, Duration, usize), &'static str) {
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
            if no_opt {
                ctx.set_optimizer_options(boa_engine::optimizer::OptimizerOptions::empty());
            }
            // Start each measured run from a reclaimed heap, so GC growth state
            // doesn't leak across runs.
            boa_engine::gc::force_collect();

            let gc0 = boa_engine::gc::gc_profile();
            let t = Instant::now();
            let script =
                boa_engine::Script::parse(Source::from_bytes(&src), None, &mut ctx).unwrap();
            let parse = t.elapsed();
            let t = Instant::now();
            script.codeblock(&mut ctx).unwrap();
            let compile = t.elapsed();
            let t = Instant::now();
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                script.evaluate(&mut ctx)
            }));
            let execute = t.elapsed();
            let gc1 = boa_engine::gc::gc_profile();
            let outcome = match res {
                Ok(Ok(_)) => "clean",
                Ok(Err(_)) => "threw",
                Err(_) => "PANIC",
            };
            (parse, compile, execute, (gc1.0 - gc0.0, gc1.1 - gc0.1, gc1.2), outcome)
        };

        // median + dispersion over a slice of durations.
        let stats = |xs: &[Duration]| -> (Duration, Duration, Duration) {
            let mut v = xs.to_vec();
            v.sort_unstable();
            (v[v.len() / 2], v[0], v[v.len() - 1]) // median, min, max
        };

        let mut parses = Vec::new();
        let mut compiles = Vec::new();
        let mut executes = Vec::new();
        let mut last_gc = (0usize, Duration::ZERO, 0usize);
        let mut last_outcome = "n/a";
        for i in 0..runs {
            let (p, c, e, gc, outcome) = run_once();
            last_gc = gc;
            last_outcome = outcome;
            eprintln!(
                "run {i:>2}{}: parse {p:>9.3?}  compile {c:>9.3?}  execute {e:>9.3?}  [{outcome}]",
                if i == 0 && runs > 1 { " (cold)" } else { "" }
            );
            // Discard the cold run only when we have warm runs to keep.
            if i > 0 || runs == 1 {
                parses.push(p);
                compiles.push(c);
                executes.push(e);
            }
        }

        let (pm, plo, phi) = stats(&parses);
        let (cm, clo, chi) = stats(&compiles);
        let (em, elo, ehi) = stats(&executes);
        eprintln!(
            "--- medians over {} warm run(s) (min … max) ---",
            parses.len()
        );
        eprintln!("parse    : {pm:>9.3?}   ({plo:.3?} … {phi:.3?})");
        eprintln!("compile  : {cm:>9.3?}   ({clo:.3?} … {chi:.3?})");
        eprintln!("execute  : {em:>9.3?}   ({elo:.3?} … {ehi:.3?})");
        eprintln!("TOTAL    : {:>9.3?}   (medians summed)", pm + cm + em);
        eprintln!("--- last run GC / footprint ---");
        eprintln!("result   : {last_outcome}");
        eprintln!(
            "gc       : {} collections, {:?}; live {} MiB",
            last_gc.0,
            last_gc.1,
            last_gc.2 / (1024 * 1024)
        );
        if let Some(peak) = proc_peak_rss_mib() {
            eprintln!("peak RSS : {peak} MiB (VmHWM)");
        }
    }

    /// Process peak resident set size (MiB) from `/proc/self/status` VmHWM —
    /// Linux only; `None` elsewhere. Cheaper and dependency-free vs getrusage
    /// (the host's `perf` counters are locked; see CLAUDE.md).
    fn proc_peak_rss_mib() -> Option<u64> {
        std::fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .map(|kib| kib / 1024)
    }

    /// The phase split inside `run_script` is exactly `ctx.eval` unrolled, so a
    /// page must run identically; and with profiling armed the whole-load
    /// parse/compile/execute accumulators populate (the Step 1 decision-gate
    /// tool). Snapshot BEFORE disarming so a failed assert can't leave the
    /// thread armed for a sibling test.
    #[test]
    fn phase_profiler_splits_a_load_and_preserves_behaviour() {
        phases_arm(true);
        let html = r#"<html><body><div id="x"></div>
            <script>document.getElementById('x').textContent = 'hi' + (1 + 1);</script>
            </body></html>"#;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        let p = phases_snapshot();
        phases_arm(false);

        assert!(
            out.contains("hi2"),
            "the script effect must survive the phase split: {out}"
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // config + prelude + the page script + DOMContentLoaded + load each run
        // through the split, so every phase fires several times.
        assert!(
            p.parse_n >= 1 && !p.parse.is_zero(),
            "parse not recorded: {p:?}"
        );
        assert!(
            p.compile_n >= 1 && !p.compile.is_zero(),
            "compile not recorded: {p:?}"
        );
        assert!(p.execute_n >= 1, "execute not recorded: {p:?}");
    }

    /// Profiling OFF (the default) records nothing — the unprofiled path pays
    /// only the cached bool check and the accumulators stay zero.
    #[test]
    fn phase_profiler_is_inert_when_disarmed() {
        phases_arm(false);
        phases_reset();
        let html = r#"<html><body><script>var x = 1 + 1;</script></body></html>"#;
        let (_out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        let p = phases_snapshot();
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(p.parse_n, 0, "armed off but parse recorded: {p:?}");
        assert_eq!(p.compile_n, 0, "armed off but compile recorded: {p:?}");
        assert_eq!(p.execute_n, 0, "armed off but execute recorded: {p:?}");
    }

    // ---- K1/K2 keystone: detachable compiled-code image ------------------
    //
    // A compiled `CodeBlock` is pinned to its compiling thread's GC heap
    // (nested functions are `Gc<CodeBlock>`) and uses non-`Send` `JsString`/
    // `Rc` throughout, so it can't be cached across pages or handed to a
    // compile worker. `CodeBlock::to_image` detaches it into an owned, `Send`
    // `CodeBlockImage`; `CodeBlock::from_image` rehydrates one on ANY thread.
    // These three tests are the feasibility proof for the cache /
    // compile-on-arrival / parallel-compile cluster (see
    // JS_ENGINE_PERFORMANCE_PLAN.md). The fixture exercises every coupled
    // payload: a nested function (recursive `Gc<CodeBlock>`), a closure over a
    // captured local (`Scope`), a `BigInt` constant, string constants, and
    // property inline caches.
    const KEYSTONE_FIXTURE: &[u8] = br#"
        function add(a, b) { return a + b; }
        function make() {
            var base = 10n;               // BigInt constant, captured by the closure
            return function (x) { return base + BigInt(x); };
        }
        var o = { tag: "trust", n: 7 };   // string constants + property inline caches
        add(2, 3) + Number(make()(o.n)) + o.tag.length
    "#;

    /// Compile a source to a detached image (the dehydrate half).
    fn keystone_image_of(src: &[u8]) -> boa_engine::vm::CodeBlockImage {
        let mut ctx = Context::default();
        let script = boa_engine::Script::parse(Source::from_bytes(src), None, &mut ctx).unwrap();
        // `codeblock` compiles + memoizes the block; dehydrate it.
        script.codeblock(&mut ctx).unwrap().to_image()
    }

    /// Dehydrate → rehydrate → dehydrate is a fixpoint: the round-trip loses
    /// nothing (bytecode, the recursive nested-function tree, scopes, the
    /// bigint, strings, bindings, the source map all survive byte-for-byte).
    #[test]
    fn code_block_image_round_trips_losslessly() {
        let image = keystone_image_of(KEYSTONE_FIXTURE);
        let rehydrated = boa_engine::vm::CodeBlock::from_image(&image);
        let image_again = rehydrated.to_image();
        assert_eq!(
            image, image_again,
            "dehydrate → rehydrate → dehydrate must be lossless"
        );
    }

    /// The image is `Send` and rehydrates into a DIFFERENT thread's own
    /// `boa_gc` heap — the cross-page-cache / compile-worker scenario, the
    /// whole point of K1. Re-dehydrating there and comparing proves the remote
    /// rehydration is faithful.
    #[test]
    fn code_block_image_is_send_and_rehydrates_on_another_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<boa_engine::vm::CodeBlockImage>();

        let image = keystone_image_of(KEYSTONE_FIXTURE);
        let expected = image.clone();
        let remote = std::thread::spawn(move || {
            // No `Context` needed: `from_image` only allocates `Gc`s into this
            // thread's heap.
            boa_engine::vm::CodeBlock::from_image(&image).to_image()
        })
        .join()
        .unwrap();
        assert_eq!(
            expected, remote,
            "rehydration on another thread must reproduce the image"
        );
    }

    /// A rehydrated block, installed as a fresh script's body in a DIFFERENT
    /// realm and evaluated, computes the identical result as the original — the
    /// exact "compile in realm A, run in realm B" path a cross-page cache uses.
    #[test]
    fn rehydrated_code_block_executes_identically() {
        // Direct: compile + run in realm A.
        let mut ctx_a = Context::default();
        let script_a =
            boa_engine::Script::parse(Source::from_bytes(KEYSTONE_FIXTURE), None, &mut ctx_a)
                .unwrap();
        let image = script_a.codeblock(&mut ctx_a).unwrap().to_image();
        let direct = script_a.evaluate(&mut ctx_a).unwrap();

        // Rehydrated: install the detached image as a fresh script's body in
        // realm B and evaluate it.
        let mut ctx_b = Context::default();
        let script_b =
            boa_engine::Script::parse(Source::from_bytes(KEYSTONE_FIXTURE), None, &mut ctx_b)
                .unwrap();
        script_b.set_codeblock(boa_engine::vm::CodeBlock::from_image(&image));
        let rehydrated = script_b.evaluate(&mut ctx_b).unwrap();

        // add(2,3)=5; Number(make()(7))=Number(10n+7n)=17; "trust".length=5.
        assert_eq!(direct.as_number(), Some(27.0), "fixture sanity check");
        assert_eq!(
            rehydrated.as_number(),
            direct.as_number(),
            "the rehydrated code block must compute the same value as the original"
        );
    }

    // ---- Step 4: prelude compile cache (the keystone's first consumer) -----
    //
    // `run_prelude` compiles the prelude ONCE per process into a `CodeBlockImage`
    // and, on every later page, rehydrates it into an empty carrier script —
    // skipping the 65 KB parse and the compile. The gate below proves the novel
    // path: the keystone tests rehydrate a block into a shell parsed from the
    // SAME source; here the shell is parsed from "" and the block comes from the
    // real prelude, in a DIFFERENT realm. If this holds, the empty-shell trick is
    // sound and needs no fork change.

    /// Build a faithful page context (DOM + syscalls + config), exactly as
    /// `load_page` does — the prelude is built on those syscalls.
    fn cache_test_ctx() -> Context {
        let dom = Rc::new(RefCell::new(Dom::parse_document(
            r#"<html><head></head><body><p>hi</p></body></html>"#,
        )));
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
        let cfg = r#"globalThis.__trust_cfg = { url: "https://example.com/", ua: "TRust/0.1", width: 800, height: 600 };"#;
        ctx.eval(Source::from_bytes(cfg.as_bytes())).unwrap();
        ctx
    }

    /// A fingerprint of the platform the prelude installs: core interfaces, a few
    /// recently-shipped ones, and a live DOM round trip through the wrapper/
    /// classList machinery, all serialized to one comparable string.
    fn prelude_fingerprint(ctx: &mut Context) -> String {
        const PROBE: &[u8] = br##"JSON.stringify({
            document: typeof document,
            querySelector: typeof document.querySelector,
            Element: typeof Element,
            Node: typeof Node,
            Event: typeof Event,
            MutationObserver: typeof MutationObserver,
            fetch: typeof fetch,
            localStorage: typeof localStorage,
            readyState: __trust.readyState,
            errors: __trust.errors.length,
            text: document.querySelector('p').textContent,
            tag: document.querySelector('p').tagName,
            classList: (function(){ var d=document.createElement('div'); d.className='a b'; return d.classList.contains('b') && !d.classList.contains('c'); })()
        })"##;
        ctx.eval(Source::from_bytes(PROBE))
            .unwrap()
            .to_string(ctx)
            .unwrap()
            .to_std_string_escaped()
    }

    /// THE Phase-1 correctness gate: a prelude run from a REHYDRATED
    /// `CodeBlockImage` installed into an EMPTY carrier script builds the
    /// byte-identical platform as a cold `eval(PRELUDE)`.
    #[test]
    fn cached_prelude_builds_the_same_platform_as_a_cold_eval() {
        // A compile-time guarantee the image can live in a shared `static
        // OnceLock` reachable from every page thread.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<boa_engine::vm::CodeBlockImage>();

        // Cold: parse + compile + run the real prelude.
        let mut cold = cache_test_ctx();
        cold.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();
        let cold_fp = prelude_fingerprint(&mut cold);

        // Detach a compiled image from one realm...
        let image = {
            let mut img_ctx = cache_test_ctx();
            let script = boa_engine::Script::parse(
                Source::from_bytes(PRELUDE.as_bytes()),
                None,
                &mut img_ctx,
            )
            .unwrap();
            script.codeblock(&mut img_ctx).unwrap().to_image()
        };

        // ...and run it via an EMPTY carrier script in a FRESH realm — the cached
        // path `run_prelude` takes on every page after the first.
        let mut cached = cache_test_ctx();
        let shell = boa_engine::Script::parse(Source::from_bytes("".as_bytes()), None, &mut cached)
            .unwrap();
        shell.set_codeblock(boa_engine::vm::CodeBlock::from_image(&image));
        shell.evaluate(&mut cached).unwrap();
        let cached_fp = prelude_fingerprint(&mut cached);

        assert_eq!(
            cold_fp, cached_fp,
            "a rehydrated prelude must build the identical platform as a cold eval"
        );
        // ...and the fingerprint is the real thing, not two matching blanks.
        assert!(
            cold_fp.contains(r#""text":"hi""#),
            "probe sanity: {cold_fp}"
        );
        assert!(cold_fp.contains(r#""tag":"P""#), "probe sanity: {cold_fp}");
        assert!(
            cold_fp.contains(r#""classList":true"#),
            "probe sanity: {cold_fp}"
        );
        assert!(
            cold_fp.contains(r#""MutationObserver":"function""#),
            "probe sanity: {cold_fp}"
        );
    }

    /// `run_prelude` itself (the production entry) installs a working platform —
    /// on the cold compile AND on the cached rehydrate. The first call populates
    /// the process-global `PRELUDE_IMAGE`, so the second is DEFINITELY the cached
    /// path regardless of cross-test ordering.
    #[test]
    fn run_prelude_installs_the_platform_cold_and_cached() {
        let budget = Budget::new(WALL_BUDGET);
        let run = || {
            let mut ctx = cache_test_ctx();
            let mut outcome = Outcome::default();
            run_prelude(&mut ctx, &budget, &mut outcome);
            (ctx, outcome)
        };

        // First call: cold-or-cached by test order, but GUARANTEES the cache is
        // populated afterwards.
        let (mut first, o1) = run();
        assert!(o1.errors.is_empty(), "{:?}", o1.errors);
        assert!(!o1.panicked);

        // Second call: DEFINITELY the cached rehydrate path.
        let (mut second, o2) = run();
        assert!(o2.errors.is_empty(), "{:?}", o2.errors);
        assert!(!o2.panicked);

        assert_eq!(
            prelude_fingerprint(&mut first),
            prelude_fingerprint(&mut second)
        );
        assert!(PRELUDE_IMAGE.get().is_some(), "cache must be populated");
    }

    // ---- Step 5a: parallel parse (raw_parse off-thread + compile_raw) ------
    //
    // `Script::raw_parse` runs the lex/parse on a worker with a PRIVATE interner
    // (the bulk of the parse phase, off the page thread); `Script::compile_raw`
    // runs the deferred scope analysis against the SHARED realm scope and
    // compiles on the page thread. These tests prove the split is behaviour-
    // identical to the sequential `Script::parse` path, including the two things
    // that make it subtle: cross-script bindings resolve correctly even though
    // each script used a different interner (the shared scope links by NAME),
    // and tagged-template call sites stay distinct across separately parsed
    // scripts (the per-script parser id).

    /// Raw-parse `src` on a SEPARATE thread — proving `RawScript: Send` and that
    /// the parse really leaves the owning thread, exactly as the pool does.
    fn raw_parse_on_worker(src: &str, id: u32) -> Result<boa_engine::RawScript, String> {
        let owned = src.to_string();
        std::thread::spawn(move || {
            boa_engine::Script::raw_parse(Source::from_bytes(owned.as_bytes()), id)
        })
        .join()
        .expect("parse worker panicked")
    }

    /// Drive `sources` (document order) through the parallel-parse path: each is
    /// raw-parsed on a worker, then compiled + evaluated in order in one context
    /// — exactly what `load_page`'s loop does — returning the last value.
    fn last_via_parallel_parse(sources: &[&str]) -> JsValue {
        let mut ctx = Context::default();
        let mut last = JsValue::undefined();
        for src in sources {
            let id = ctx.next_parser_identifier();
            let raw = raw_parse_on_worker(src, id).expect("raw parse failed");
            let script = boa_engine::Script::compile_raw(raw, None, &mut ctx).expect("compile_raw");
            last = script.evaluate(&mut ctx).expect("evaluate");
        }
        last
    }

    /// The same `sources` the ORIGINAL way (sequential `Script::parse` +
    /// evaluate, one shared interner) — the equivalence baseline.
    fn last_via_sequential_parse(sources: &[&str]) -> JsValue {
        let mut ctx = Context::default();
        let mut last = JsValue::undefined();
        for src in sources {
            let script =
                boa_engine::Script::parse(Source::from_bytes(src.as_bytes()), None, &mut ctx)
                    .unwrap();
            last = script.evaluate(&mut ctx).unwrap();
        }
        last
    }

    /// A single script: the off-thread raw parse + swapped-interner compile must
    /// compute the identical result as the in-context `Script::parse` path. The
    /// keystone fixture exercises bigint, closures, string constants, and ICs.
    #[test]
    fn raw_parse_compile_matches_direct_parse() {
        let mut ctx_seq = Context::default();
        let direct =
            boa_engine::Script::parse(Source::from_bytes(KEYSTONE_FIXTURE), None, &mut ctx_seq)
                .unwrap()
                .evaluate(&mut ctx_seq)
                .unwrap();

        let mut ctx_par = Context::default();
        let id = ctx_par.next_parser_identifier();
        let raw = raw_parse_on_worker(std::str::from_utf8(KEYSTONE_FIXTURE).unwrap(), id).unwrap();
        let prepared = boa_engine::Script::compile_raw(raw, None, &mut ctx_par)
            .unwrap()
            .evaluate(&mut ctx_par)
            .unwrap();

        assert_eq!(direct.as_number(), Some(27.0), "fixture sanity check");
        assert_eq!(
            prepared.as_number(),
            direct.as_number(),
            "off-thread raw parse + compile_raw must match Script::parse"
        );
    }

    /// Scripts that lean on each other across the boundary: a global `var`
    /// (resolved by name on the global object), top-level `let`/`const`
    /// (resolved by SLOT INDEX into one shared global lexical env, so the
    /// cross-script declaration order must be preserved), a function declared in
    /// one and called in another, and a property name interned separately in
    /// each script (the shared scope must link them by name, not interner index).
    /// The parallel path must reproduce the sequential result exactly.
    #[test]
    fn parallel_parse_preserves_cross_script_globals() {
        let sources = [
            r#"globalThis.out = "";
               var v = 10;
               let lx = 5;
               function bump(o){ return o.count + 1; }"#,
            r#"var w = v + 20;          // reads script 1's global var
               let ly = lx + 7;         // reads script 1's global let; new slot AFTER lx
               const obj = { count: 100 };"#,
            r#"[v, w, lx, ly, bump(obj)].join(",")"#,
        ];
        let parallel = last_via_parallel_parse(&sources)
            .as_string()
            .expect("a string result")
            .to_std_string_lossy();
        let sequential = last_via_sequential_parse(&sources)
            .as_string()
            .expect("a string result")
            .to_std_string_lossy();
        assert_eq!(parallel, "10,30,5,12,101", "parallel-parse result wrong");
        assert_eq!(
            parallel, sequential,
            "parallel parse must match sequential parse exactly"
        );
    }

    // ---- Phase 2: cross-page CDN compile cache --------------------------
    //
    // An external classic library compiled on one page is dehydrated to a
    // `CodeBlockImage` and rehydrated on later pages (no re-parse, no
    // re-compile). The realm-portability GATE decides what is safe to reuse: a
    // classic script runs in the page's global scope, and a top-level
    // `let`/`const`/`class` binds a SLOT INDEX into the realm's shared,
    // accumulating global declarative environment — page-position-dependent, so
    // its block is not reusable elsewhere. The gate admits only blocks that
    // neither create such a binding (`global_declarations_are_replayable`) nor
    // read a prior script's (`is_realm_portable`).

    /// The gate's two fork checks classify libraries correctly: a UMD/IIFE bundle
    /// (incl. one assigned to a top-level `var`) is cacheable; a top-level global
    /// LEXICAL (`let`/`const`/`class`) is rejected by
    /// `global_declarations_are_replayable` (it adds a shared-scope slot the
    /// cache path would skip); and a block that merely READS a prior script's
    /// global lexical is caught by the defensive `is_realm_portable` scan even
    /// though it declares nothing of its own.
    #[test]
    fn cdn_cache_gate_classifies_realm_portability() {
        fn gate(ctx: &mut Context, src: &[u8]) -> (bool, bool) {
            let script = boa_engine::Script::parse(Source::from_bytes(src), None, ctx).unwrap();
            let cb = script.codeblock(ctx).unwrap();
            (
                script.global_declarations_are_replayable(),
                cb.is_realm_portable(),
            )
        }
        // Cacheable shapes: a bare IIFE, and `var Lib = (function(){…})()` (Vue's
        // global build) — globals created by name, no lexical slot.
        for src in [
            &br#"(function(g){ g.__lib = (typeof someUndeclared); })(globalThis);"#[..],
            br#"var Lib = (function(){ return { v: 1 }; })();"#,
            br#"function f(){ return 1; } globalThis.x = f();"#,
        ] {
            assert_eq!(
                gate(&mut Context::default(), src),
                (true, true),
                "a by-name-global bundle must be realm-portable: {}",
                String::from_utf8_lossy(src)
            );
        }
        // Global LEXICAL declarations add a slot to the shared global declarative
        // scope at analysis time → not replayable from bytecode → rejected.
        for src in [
            &b"const X = 1; (function(){})();"[..],
            b"let Y = 1;",
            b"class Z {}",
        ] {
            assert!(
                !gate(&mut Context::default(), src).0,
                "a top-level lexical declaration must be rejected: {}",
                String::from_utf8_lossy(src)
            );
        }
        // The defensive scan: a library that READS a prior script's global
        // lexical resolves it by SLOT (scope==1) — it declares nothing itself,
        // but its block is not realm-portable.
        let mut shared = Context::default();
        boa_engine::Script::parse(Source::from_bytes(b"const SHARED = 7;"), None, &mut shared)
            .unwrap()
            .evaluate(&mut shared)
            .unwrap();
        let (replayable, portable) = gate(&mut shared, br#"(function(){ return SHARED + 1; })();"#);
        assert!(replayable, "the library itself declares no global lexicals");
        assert!(
            !portable,
            "reading a prior script's global lexical (a slot) is not realm-portable"
        );
    }

    /// Top-level `var`/`function` globals are created BY NAME from the bytecode,
    /// so a rehydrated block recreates them and a LATER script resolves them by
    /// name — on both the cold page and the cached page. This is what lets Vue's
    /// `var Vue = …` global build be cached; the test pins the soundness of
    /// admitting by-name globals (vs. only bare IIFEs).
    #[test]
    fn cdn_cache_replays_top_level_var_and_function_globals() {
        let budget = Budget::new(WALL_BUDGET);
        let lib = br#"var LibVer = 7; function libDouble(n){ return n*2; }"#;
        let key = cdn_cache_key(lib);
        let url = "https://cdn.example/varlib.js";

        // Page A: miss → compile → store (replayable + portable) → run.
        let mut a = Context::default();
        let mut oa = Outcome::default();
        run_external_classic(&mut a, url, lib, None, &budget, &mut oa);
        assert!(oa.errors.is_empty(), "{:?}", oa.errors);
        assert!(
            matches!(cdn_cache_lookup(&key), CdnLookup::Reusable(_)),
            "a var/function (by-name) library must be cacheable"
        );
        // A later script on page A reads the globals by name.
        assert_eq!(
            a.eval(Source::from_bytes(b"libDouble(LibVer)"))
                .unwrap()
                .as_number(),
            Some(14.0),
            "var/function globals must resolve on page A"
        );

        // Page B (fresh realm): cache HIT → rehydrate. Replaying the block must
        // recreate the globals, and a later script must resolve them by name.
        let mut b = Context::default();
        let mut ob = Outcome::default();
        run_external_classic(&mut b, url, lib, None, &budget, &mut ob);
        assert!(ob.errors.is_empty(), "{:?}", ob.errors);
        assert_eq!(
            b.eval(Source::from_bytes(b"libDouble(LibVer)"))
                .unwrap()
                .as_number(),
            Some(14.0),
            "rehydrated var/function globals must resolve by name on page B"
        );
    }

    /// End to end through `run_external_classic`: an IIFE library MISSES on page
    /// A (compile → store the detached image → run), then HITS on page B (a fresh
    /// realm), where it is rehydrated and runs identically — no re-parse, no
    /// re-compile. Unique body so the process-global cache can't collide with
    /// another test.
    #[test]
    fn cdn_cache_reuses_a_library_across_pages() {
        let budget = Budget::new(WALL_BUDGET);
        let lib =
            br#"(function(){ globalThis.__cdn_reuse = (globalThis.__cdn_reuse||0) + 41; })();"#;
        let key = cdn_cache_key(lib);
        let url = "https://cdn.example/reuse.js";

        // Page A: a true miss → compile + store + run.
        let mut a = Context::default();
        let mut oa = Outcome::default();
        run_external_classic(&mut a, url, lib, None, &budget, &mut oa);
        assert!(oa.errors.is_empty(), "{:?}", oa.errors);
        assert_eq!(
            a.eval(Source::from_bytes(b"globalThis.__cdn_reuse"))
                .unwrap()
                .as_number(),
            Some(41.0),
            "the library must run on page A"
        );
        assert!(
            matches!(cdn_cache_lookup(&key), CdnLookup::Reusable(_)),
            "an IIFE library must be cached as reusable after page A"
        );

        // Page B: a fresh realm, now a cache HIT → rehydrate (carrier path).
        let mut b = Context::default();
        let mut ob = Outcome::default();
        run_external_classic(&mut b, url, lib, None, &budget, &mut ob);
        assert!(ob.errors.is_empty(), "{:?}", ob.errors);
        assert_eq!(
            b.eval(Source::from_bytes(b"globalThis.__cdn_reuse"))
                .unwrap()
                .as_number(),
            Some(41.0),
            "the rehydrated library must run identically on page B"
        );
    }

    /// A non-portable library (a top-level `let`) is run correctly but marked
    /// `NotReusable`, so it is never rehydrated and the gate isn't re-evaluated
    /// on later pages.
    #[test]
    fn cdn_cache_marks_non_portable_library_not_reusable() {
        let budget = Budget::new(WALL_BUDGET);
        let lib = br#"let __cdn_np = 5; globalThis.__cdn_np_marker = __cdn_np + 1;"#;
        let key = cdn_cache_key(lib);
        let mut a = Context::default();
        let mut oa = Outcome::default();
        run_external_classic(
            &mut a,
            "https://cdn.example/np.js",
            lib,
            None,
            &budget,
            &mut oa,
        );
        assert!(oa.errors.is_empty(), "{:?}", oa.errors);
        assert_eq!(
            a.eval(Source::from_bytes(b"globalThis.__cdn_np_marker"))
                .unwrap()
                .as_number(),
            Some(6.0),
            "the library must still run correctly"
        );
        assert!(
            matches!(cdn_cache_lookup(&key), CdnLookup::NotReusable),
            "a global-lexical library must be marked NotReusable, not cached"
        );
    }

    /// `cdn_cache_hits` (which keeps rehydrated scripts OUT of the parse pool)
    /// flags an external classic only once its image is cached.
    #[test]
    fn cdn_cache_hits_flags_only_cached_externals() {
        let budget = Budget::new(WALL_BUDGET);
        let lib = br#"(function(){ globalThis.__cdn_hits = 1; })();"#;
        let url = "https://cdn.example/hits.js";
        let scripts = vec![(Some(url.to_string()), String::new(), None, 0usize)];
        let externals = vec![(url.to_string(), Some(lib.to_vec()))];

        // Uncached → not a hit (so it WOULD go to the parse pool).
        assert!(
            cdn_cache_hits(&scripts, &externals).is_empty(),
            "an uncached library must not be flagged as a hit"
        );
        // Warm the cache.
        let mut a = Context::default();
        let mut oa = Outcome::default();
        run_external_classic(&mut a, url, lib, None, &budget, &mut oa);
        assert!(oa.errors.is_empty(), "{:?}", oa.errors);
        // Cached → flagged (so it's rehydrated, not re-parsed).
        assert!(
            cdn_cache_hits(&scripts, &externals).contains(&0),
            "a cached library must be flagged for rehydration"
        );
    }

    /// The real CLASSIC CDN libraries this feature targets must pass the
    /// realm-portability gate (else the cache does nothing for them). Ignored
    /// like the canaries — it reads the bundles from `target/canary/`. Run with
    /// `cargo test --release cdn_cache_admits_real_cdn_bundles -- --ignored
    /// --nocapture`. (Lit ships only as an ES module — `<script type=module>`,
    /// out of scope for this classic-script cache — so it can't be parsed as a
    /// `Script` and is reported as such, not asserted.)
    #[test]
    #[ignore = "needs the canary bundles in target/canary/"]
    fn cdn_cache_admits_real_cdn_bundles() {
        // The classic (UMD/IIFE) bundles the cache is meant to admit.
        let classic = ["jquery-3.7.1.min.js", "d3.v7.min.js", "vue.global.prod.js"];
        for name in classic
            .iter()
            .copied()
            .chain(["lit-core.min.js", "react.production.min.js"])
        {
            let Ok(src) = std::fs::read(format!("target/canary/{name}")) else {
                eprintln!("{name}: SKIP (not present)");
                continue;
            };
            let mut ctx = Context::default();
            let Ok(script) = boa_engine::Script::parse(Source::from_bytes(&src), None, &mut ctx)
            else {
                eprintln!("{name}: not a classic script (ES module?) — out of scope, skipped");
                assert!(
                    !classic.contains(&name),
                    "{name} was expected to be a classic script"
                );
                continue;
            };
            let cb = script.codeblock(&mut ctx).unwrap();
            let replayable = script.global_declarations_are_replayable();
            let portable = cb.is_realm_portable();
            eprintln!(
                "{name}: replayable={replayable} is_realm_portable={portable} => cacheable={}",
                replayable && portable
            );
            assert!(
                replayable && portable,
                "{name} must be realm-portable for the CDN cache to help it"
            );
        }
    }

    /// Two scripts with the SAME tag function and SAME template content. Their
    /// tagged-template call sites are distinct, so per spec they produce distinct
    /// (frozen) template arrays — but only if each script parsed with a unique
    /// parser id; a collision would make them share one cached template object.
    /// This guards the per-script id allocation through the pool.
    #[test]
    fn parallel_parse_keeps_tagged_template_sites_distinct() {
        let sources = [
            r#"function tag(s){ return s; } globalThis.A = tag`shared`;"#,
            r#"function tag(s){ return s; } globalThis.B = tag`shared`;
               globalThis.A !== globalThis.B"#,
        ];
        assert_eq!(
            last_via_parallel_parse(&sources).as_boolean(),
            Some(true),
            "tagged-template sites in separately parsed scripts must stay distinct"
        );
    }

    /// The immutable-property fast paths (constant `nodeType` per wrapper class,
    /// lazily cached tag) must return the SAME values the per-access syscalls
    /// did. These are the getters jQuery's `each`/`data`/`add` hammer; the
    /// MO/jQuery hot-path pass made them syscall-free. Guards the constants and
    /// the cache against a future wrong value.
    #[test]
    fn node_type_and_tag_getters_stay_correct() {
        let html = r##"<html><body><div id=o></div><script>
            const el = document.createElement('P');
            const tx = document.createTextNode('hi');
            const cm = document.createComment('c');
            const fr = document.createDocumentFragment();
            const ok =
                el.nodeType === 1 && tx.nodeType === 3 && cm.nodeType === 8 &&
                fr.nodeType === 11 && document.nodeType === 9 &&
                // Element name getters agree and derive from the (cached) tag.
                el.nodeName === el.tagName && el.tagName === el.localName.toUpperCase() &&
                // Repeated access is stable (cache returns the same value).
                el.tagName === el.tagName && el.localName === el.localName &&
                // Non-element node names are the spec constants.
                tx.nodeName === "#text" && cm.nodeName === "#comment" &&
                fr.nodeName === "#document-fragment" && document.nodeName === "#document";
            document.getElementById('o').setAttribute('data-ok', String(ok));
        </script></body></html>"##;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains(r#"data-ok="true""#),
            "node type/name getters returned a wrong value: {out}"
        );
    }

    #[test]
    fn getattribute_cache_reflects_writes() {
        // The per-element read cache (`__ac`) must never go stale: every
        // attribute write nukes it, so a read after a set/remove — through ANY
        // writer that funnels into setAttribute/removeAttribute (className,
        // classList, dataset, style) — sees the new value. Mixed-case access
        // stays correct too (Rust matches attribute names case-insensitively).
        let html = r##"<html><body><div id=o></div><div id=t class="a" data-x="1" title="hi"></div><script>
            const el = document.getElementById('t'), log = [];
            log.push(el.getAttribute('class'));   // a   (cold)
            log.push(el.getAttribute('class'));    // a   (cache hit)
            el.setAttribute('class', 'b');
            log.push(el.getAttribute('class'));    // b   (cache dropped)
            el.className = 'c';                    // className -> setAttribute
            log.push(el.getAttribute('class'));    // c
            el.classList.add('d');                 // classList -> setAttribute
            log.push(el.getAttribute('class'));    // c d
            el.dataset.x = '2';                    // dataset -> setAttribute
            log.push(el.getAttribute('data-x'));   // 2
            el.style.color = 'x';                  // an unrelated write must not corrupt other reads
            log.push(el.getAttribute('data-x'));   // 2 (re-fetched correctly)
            log.push(String(el.getAttribute('title') === el.getAttribute('TITLE'))); // case-insensitive agree
            el.setAttribute('title', 'bye');
            log.push(el.getAttribute('TITLE'));    // bye (mixed-case read not stale after lowercase write)
            el.removeAttribute('title');
            log.push(String(el.getAttribute('title'))); // null
            document.getElementById('o').textContent = log.join('|');
        </script></body></html>"##;
        let (out, outcome) = page(html);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("a|a|b|c|c d|2|2|true|bye|null"),
            "attribute read cache went stale: {out}"
        );
    }

    #[test]
    fn base_href_cache_invalidates_on_nav_and_base_changes() {
        // The cached base URL must refresh on (a) a location change, (b) a
        // runtime <base> insertion, and (c) a <base href> mutation. page()
        // loads at https://example.com/a/page, so "foo" resolves there first.
        let html = r##"<html><body><div id=o></div><a id=a href="foo">x</a><script>
            const a = document.getElementById('a'), log = [];
            log.push(a.href);                       // -> https://example.com/a/foo
            log.push(a.href);                       // cache hit, identical
            history.pushState(null, '', '/x/y');    // location change nulls the base cache
            log.push(a.href);                       // -> https://example.com/x/foo
            const b = document.createElement('base');
            b.setAttribute('href', 'https://cdn.example.com/p/');
            document.body.appendChild(b);           // runtime <base> insertion
            log.push(a.href);                       // -> https://cdn.example.com/p/foo
            b.setAttribute('href', 'https://cdn.example.com/q/'); // mutate the base
            log.push(a.href);                       // -> https://cdn.example.com/q/foo
            document.getElementById('o').textContent = log.join(' ');
        </script></body></html>"##;
        let (out, outcome) = page(html);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("https://example.com/a/foo https://example.com/a/foo https://example.com/x/foo https://cdn.example.com/p/foo https://cdn.example.com/q/foo"),
            "base href cache did not invalidate: {out}"
        );
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
    fn window_post_message_delivers_to_self_as_a_message_event() {
        // `window.postMessage` (HTML web messaging). With no foreign frames the
        // only target is ourselves, so the message arrives asynchronously as a
        // `message` MessageEvent carrying its data. Steam's focus-restore
        // handshake posts a string to itself and listens for it; a missing
        // `window.postMessage` was an uncaught TypeError there.
        let dom = Rc::new(RefCell::new(Dom::parse_document(
            r#"<html><head></head><body></body></html>"#,
        )));
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
        let cfg = r#"globalThis.__trust_cfg = { url: "https://example.com/", ua: "TRust/0.1", width: 800, height: 600 };"#;
        ctx.eval(Source::from_bytes(cfg.as_bytes())).unwrap();
        ctx.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();

        let budget = Budget::new(WALL_BUDGET);
        let mut outcome = Outcome::default();
        run_script(
            &mut ctx,
            "pm.js",
            br##"
            globalThis.out = {};
            window.addEventListener("message", function (e) {
                out.data = e.data;
                out.type = e.type;
                out.isMessageEvent = (e instanceof MessageEvent);
                out.selfSource = (e.source === window);
            });
            // Delivery is async (a task): not seen synchronously.
            window.postMessage("FocusRestoreReady");
            out.syncSeen = ("data" in out);
            "##,
            &budget,
            &mut outcome,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // Delivered on a macrotask (setTimeout 0): advance virtual time.
        settle(&mut ctx, &budget, 4, &mut outcome);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let s = |ctx: &mut Context, expr: &[u8]| {
            ctx.eval(Source::from_bytes(expr))
                .unwrap()
                .to_string(ctx)
                .unwrap()
                .to_std_string_escaped()
        };
        assert_eq!(s(&mut ctx, b"String(out.syncSeen)"), "false");
        assert_eq!(s(&mut ctx, b"out.data"), "FocusRestoreReady");
        assert_eq!(s(&mut ctx, b"out.type"), "message");
        assert_eq!(s(&mut ctx, b"String(out.isMessageEvent)"), "true");
        assert_eq!(s(&mut ctx, b"String(out.selfSource)"), "true");
    }

    #[test]
    fn file_reader_reads_blobs_asynchronously() {
        // FileReader is built by PRELUDE on the syscalls, so the test needs a
        // faithful page context (DOM + syscalls + config + PRELUDE), exactly
        // as `load_page` builds it — a bare `page_context()` has no prelude.
        let dom = Rc::new(RefCell::new(Dom::parse_document(
            r#"<html><head></head><body></body></html>"#,
        )));
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
        let cfg = r#"globalThis.__trust_cfg = { url: "https://example.com/", ua: "TRust/0.1", width: 800, height: 600 };"#;
        ctx.eval(Source::from_bytes(cfg.as_bytes())).unwrap();
        ctx.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();

        let budget = Budget::new(WALL_BUDGET);
        let mut outcome = Outcome::default();
        run_script(
            &mut ctx,
            "fr.js",
            br##"
            globalThis.out = {};
            // readAsText returns the blob's text via an on* handler.
            const r1 = new FileReader();
            r1.onload = function () { out.text = r1.result; out.state = r1.readyState; };
            r1.readAsText(new Blob(["hello world"], { type: "text/plain" }));
            // readAsDataURL via addEventListener; UTF-8 bytes -> base64.
            const r2 = new FileReader();
            r2.addEventListener("load", () => { out.url = r2.result; });
            r2.readAsDataURL(new Blob(["\u00e9"]));
            // The legacy constants exist on both the instance and constructor.
            out.consts = (new FileReader().DONE === 2 && FileReader.EMPTY === 0 && FileReader.LOADING === 1);
            "##,
            &budget,
            &mut outcome,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // Reads settle on a macrotask (setTimeout 0): advance virtual time.
        settle(&mut ctx, &budget, 4, &mut outcome);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let s = |ctx: &mut Context, expr: &[u8]| {
            ctx.eval(Source::from_bytes(expr))
                .unwrap()
                .to_string(ctx)
                .unwrap()
                .to_std_string_escaped()
        };
        assert_eq!(s(&mut ctx, b"out.text"), "hello world");
        // "é" is UTF-8 C3 A9 -> base64 w6k=; default type is octet-stream.
        assert_eq!(
            s(&mut ctx, b"out.url"),
            "data:application/octet-stream;base64,w6k="
        );
        assert_eq!(s(&mut ctx, b"String(out.state)"), "2");
        assert!(
            ctx.eval(Source::from_bytes(b"out.consts"))
                .unwrap()
                .to_boolean()
        );
    }

    /// A spec TreeWalker with a NodeFilter callback drives Primer React's
    /// focus zone (`createTreeWalker(root, SHOW_ELEMENT, {acceptNode})` +
    /// `firstChild()`/`nextNode()`). A walker missing those traversal methods
    /// or ignoring the filter threw during GitHub's render → "Unable to load
    /// page." This walks a tree collecting only elements the filter accepts.
    /// `css_bake` applies a page's CSS with no JS at all — the path for
    /// no-`<script>` pages, `set js off`, and the JS-load-timeout fallback
    /// (a big GitHub code file). It must bake cascaded properties (so the
    /// layout flexes/grids per the page) and drop hidden subtrees (so a
    /// `visibility:hidden` nav menu collapses instead of rendering expanded).
    #[test]
    fn css_bake_applies_the_cascade_without_js() {
        let html = r#"<html><head><style>
            .row { display: flex; }
            @media (min-width: 200px) { .menu { visibility: hidden; } }
        </style></head><body>
            <div class="row"><span>left</span><span>right</span></div>
            <div class="menu"><a>SECRET-MENU-ITEM</a></div>
        </body></html>"#;
        // 200 cols * 8px = 1600px wide, so the @media (min-width:200px) matches.
        let out = css_bake(html, &[], (200, 50), (8, 16));
        assert!(
            out.contains("display:flex"),
            "flex should be baked onto .row: {out}"
        );
        assert!(
            !out.contains("SECRET-MENU-ITEM"),
            "a visibility:hidden subtree must be dropped (menu collapses): {out}"
        );
    }

    #[test]
    fn tree_walker_honors_the_filter_and_traverses() {
        let dom = Rc::new(RefCell::new(Dom::parse_document(
            r#"<html><head></head><body>
               <div id="root">
                 <button class="ok">A</button>
                 <span><a class="ok">B</a><i>skip</i></span>
                 <button class="ok">C</button>
               </div></body></html>"#,
        )));
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
        let cfg = r#"globalThis.__trust_cfg = { url: "https://example.com/", ua: "TRust/0.1", width: 800, height: 600 };"#;
        ctx.eval(Source::from_bytes(cfg.as_bytes())).unwrap();
        ctx.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();

        // Collect, in document order, every element with class "ok" — exactly
        // the firstChild()+nextNode()+acceptNode pattern Primer uses.
        let probe = br#"
            (function () {
                const root = document.getElementById("root");
                const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                    acceptNode: (n) => n.classList && n.classList.contains("ok")
                        ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP,
                });
                const out = [];
                let n = w.firstChild();
                while (n) { out.push(n.textContent); n = w.nextNode(); }
                return out.join(",");
            })()
        "#;
        let v = ctx
            .eval(Source::from_bytes(probe))
            .unwrap()
            .to_string(&mut ctx)
            .unwrap()
            .to_std_string_escaped();
        assert_eq!(v, "A,B,C", "TreeWalker filtered traversal wrong");
    }

    /// The Performance Timeline getEntries* methods must exist and return
    /// arrays. GitHub's React Router calls `performance.getEntriesByName(url,
    /// "resource")` during render; a missing method is a TypeError its
    /// top-level error boundary catches, blanking the page with "Unable to
    /// load page." PRELUDE-built, so this needs a faithful page context.
    #[test]
    fn performance_timeline_methods_exist_and_return_arrays() {
        let dom = Rc::new(RefCell::new(Dom::parse_document(
            r#"<html><head></head><body></body></html>"#,
        )));
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
        let cfg = r#"globalThis.__trust_cfg = { url: "https://example.com/", ua: "TRust/0.1", width: 800, height: 600 };"#;
        ctx.eval(Source::from_bytes(cfg.as_bytes())).unwrap();
        ctx.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();

        let probe = br#"
            (function () {
                const p = performance;
                const arr = (v) => Array.isArray(v);
                const ok = typeof p.getEntriesByName === "function"
                    && typeof p.getEntries === "function"
                    && typeof p.getEntriesByType === "function"
                    && arr(p.getEntriesByName("https://x/y", "resource"))
                    && arr(p.getEntries())
                    && arr(p.getEntriesByType("resource"))
                    && typeof p.clearMarks === "function"
                    && typeof p.clearResourceTimings === "function"
                    && typeof PerformanceObserver === "function";
                // A PerformanceObserver must construct + observe without throwing.
                new PerformanceObserver(() => {}).observe({ type: "resource" });
                return ok;
            })()
        "#;
        let v = ctx.eval(Source::from_bytes(probe)).unwrap();
        assert!(v.to_boolean(), "performance timeline shim incomplete");
    }

    #[test]
    fn request_and_response_are_constructable_and_consumable() {
        // The Fetch API Request/Response globals are PRELUDE-built, so this
        // needs a faithful page context (DOM + syscalls + config + PRELUDE).
        // GitHub's react-core throws a bare `Request is not defined` without it.
        let dom = Rc::new(RefCell::new(Dom::parse_document(
            r#"<html><head></head><body></body></html>"#,
        )));
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
        let cfg = r#"globalThis.__trust_cfg = { url: "https://example.com/", ua: "TRust/0.1", width: 800, height: 600 };"#;
        ctx.eval(Source::from_bytes(cfg.as_bytes())).unwrap();
        ctx.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();

        let budget = Budget::new(WALL_BUDGET);
        let mut outcome = Outcome::default();
        run_script(
            &mut ctx,
            "req.js",
            br##"
            globalThis.out = {};
            const req = new Request("https://example.com/api", {
                method: "post", headers: { "X-Test": "1" }, body: '{"a":1}',
            });
            out.url = req.url;
            out.method = req.method;          // upper-cased
            out.hdr = req.headers.get("x-test");
            out.isReq = req instanceof Request;
            // clone() copies fields and is a fresh Request
            const c = req.clone();
            out.cloneOk = (c instanceof Request) && c !== req && c.method === "POST";
            // a Request accepted as fetch input would carry its body
            req.text().then((t) => { out.body = t; });

            const res = new Response('{"b":2}', { status: 201, statusText: "Created", headers: { "Content-Type": "application/json" } });
            out.status = res.status;
            out.ok = res.ok;                  // 201 -> true
            out.isRes = res instanceof Response;
            res.json().then((j) => { out.json = j.b; });

            const r404 = new Response("nope", { status: 404 });
            out.ok404 = r404.ok;              // false
            const rj = Response.json({ z: 9 });
            out.rjType = rj.headers.get("content-type");
            const rr = Response.redirect("https://example.com/x", 301);
            out.loc = rr.headers.get("location");
            out.rrStatus = rr.status;
            "##,
            &budget,
            &mut outcome,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // Body reads resolve as microtasks; drain to settle them.
        settle(&mut ctx, &budget, 2, &mut outcome);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let s = |ctx: &mut Context, expr: &[u8]| {
            ctx.eval(Source::from_bytes(expr))
                .unwrap()
                .to_string(ctx)
                .unwrap()
                .to_std_string_escaped()
        };
        assert_eq!(s(&mut ctx, b"out.url"), "https://example.com/api");
        assert_eq!(s(&mut ctx, b"out.method"), "POST");
        assert_eq!(s(&mut ctx, b"out.hdr"), "1");
        assert_eq!(s(&mut ctx, b"String(out.isReq)"), "true");
        assert_eq!(s(&mut ctx, b"String(out.cloneOk)"), "true");
        assert_eq!(s(&mut ctx, b"out.body"), "{\"a\":1}");
        assert_eq!(s(&mut ctx, b"String(out.status)"), "201");
        assert_eq!(s(&mut ctx, b"String(out.ok)"), "true");
        assert_eq!(s(&mut ctx, b"String(out.isRes)"), "true");
        assert_eq!(s(&mut ctx, b"String(out.json)"), "2");
        assert_eq!(s(&mut ctx, b"String(out.ok404)"), "false");
        assert_eq!(s(&mut ctx, b"out.rjType"), "application/json");
        assert_eq!(s(&mut ctx, b"out.loc"), "https://example.com/x");
        assert_eq!(s(&mut ctx, b"String(out.rrStatus)"), "301");
    }

    #[test]
    fn window_open_returns_a_stub_and_never_throws() {
        // A missing `window.open` was an uncaught TypeError that aborted click
        // handlers mid-flow (erome's age gate calls `window.open(url)` then
        // `location.href = ...`; the throw killed the navigation). It must
        // return a non-null window-like stub (code chains `.focus()`) and not
        // navigate. PRELUDE-built, so this needs a faithful page context.
        let dom = Rc::new(RefCell::new(Dom::parse_document(
            r#"<html><head></head><body></body></html>"#,
        )));
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
        let cfg = r#"globalThis.__trust_cfg = { url: "https://example.com/", ua: "TRust/0.1", width: 800, height: 600 };"#;
        ctx.eval(Source::from_bytes(cfg.as_bytes())).unwrap();
        ctx.eval(Source::from_bytes(PRELUDE.as_bytes())).unwrap();

        let budget = Budget::new(WALL_BUDGET);
        let mut outcome = Outcome::default();
        run_script(
            &mut ctx,
            "open.js",
            br##"
            globalThis.out = {};
            // The erome shape: open a popup, then keep using the page. Neither
            // call may throw, and window.open must hand back a usable object.
            const w = window.open("https://example.com/o/p-1", "_blank");
            out.notNull = (w !== null && w !== undefined);
            w.focus(); w.close();              // chained calls must be callable
            out.closed = w.closed;
            out.href = w.location.href;         // the stub remembers the url
            // window.open() with no args is valid too.
            out.bare = (typeof window.open() === "object");
            "##,
            &budget,
            &mut outcome,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let b = |ctx: &mut Context, e: &[u8]| ctx.eval(Source::from_bytes(e)).unwrap().to_boolean();
        let s = |ctx: &mut Context, e: &[u8]| {
            ctx.eval(Source::from_bytes(e))
                .unwrap()
                .to_string(ctx)
                .unwrap()
                .to_std_string_escaped()
        };
        assert!(b(&mut ctx, b"out.notNull"), "window.open returns non-null");
        assert!(b(&mut ctx, b"out.closed"), "close() sets closed");
        assert!(b(&mut ctx, b"out.bare"), "window.open() with no args works");
        assert_eq!(s(&mut ctx, b"out.href"), "https://example.com/o/p-1");
    }

    #[test]
    fn parse_header_blob_splits_name_value_pairs() {
        assert!(parse_header_blob("").is_empty());
        assert_eq!(
            parse_header_blob("x-requested-with\nXMLHttpRequest\nauthorization\nBearer t"),
            vec![
                ("x-requested-with".to_string(), "XMLHttpRequest".to_string()),
                ("authorization".to_string(), "Bearer t".to_string()),
            ]
        );
        // Empty name dropped; an odd trailing key (no value) ignored.
        assert_eq!(
            parse_header_blob("\nv\nk2\nv2\nk3"),
            vec![("k2".to_string(), "v2".to_string())]
        );
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

    /// A class field initializer that references an enclosing-function binding
    /// must see that binding as ESCAPING — the initializer is compiled to its
    /// own CodeBlock, so a "local" (register) binding there has no allocated
    /// slot and the engine emits a guaranteed `access of uninitialized binding`
    /// throw. This was the GitHub code-view / TanStack Query boot failure
    /// (`new class { #S = i }` inside a webpack module function). Fixed in the
    /// boa_ast fork: `BindingEscapeAnalyzer` now enters the field's own function
    /// scope. See [[boa-cyclic-module-debug-crash]] #10.
    #[test]
    fn class_field_initializer_capturing_an_enclosing_var_is_not_uninitialized() {
        // (name, src, expected String(RESULT)) — every case must run clean.
        let cases: &[(&str, &str, &str)] = &[
            // The minimal TanStack repro: private field = outer function var.
            (
                "private-field",
                r#"function f(){ var i={a:1}; var r=new class{ #S=i; get(){return this.#S.a} }; globalThis.RESULT=r.get(); } f();"#,
                "1",
            ),
            // Public field too (same scope mechanism).
            (
                "public-field",
                r#"function f(){ var i={a:7}; var r=new class{ S=i; }; globalThis.RESULT=r.S.a; } f();"#,
                "7",
            ),
            // Outer binding is a `let` rather than `var`.
            (
                "let-capture",
                r#"function f(){ let i={a:3}; var r=new class{ #S=i; get(){return this.#S.a} }; globalThis.RESULT=r.get(); } f();"#,
                "3",
            ),
            // Class declaration form (not just `new class{}` expression).
            (
                "class-declaration",
                r#"function f(){ var i={a:9}; class C{ #S=i; get(){return this.#S.a} } globalThis.RESULT=new C().get(); } f();"#,
                "9",
            ),
            // Static field initializer capturing an enclosing var.
            (
                "static-field",
                r#"function f(){ var i={a:5}; class C{ static S=i; } globalThis.RESULT=C.S.a; } f();"#,
                "5",
            ),
        ];
        for (name, src, expected) in cases {
            let mut ctx = page_context();
            let budget = Budget::new(WALL_BUDGET);
            let mut outcome = Outcome::default();
            run_script(&mut ctx, name, src.as_bytes(), &budget, &mut outcome);
            assert!(
                outcome.errors.is_empty(),
                "{name}: unexpected errors: {:?}",
                outcome.errors
            );
            let result = ctx
                .eval(Source::from_bytes(b"String(globalThis.RESULT)"))
                .ok()
                .and_then(|v| v.to_string(&mut ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_else(|| "<none>".into());
            assert_eq!(result, *expected, "{name}: wrong result");
        }
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

    /// Stimulus is the purest MutationObserver-driven framework: it connects a
    /// controller to any element that GAINS `data-controller`, via a
    /// MutationObserver watching the document. Here the element is injected
    /// AFTER `Application.start()` is already observing, so ONLY our
    /// MutationObserver can connect it — `connect()` running proves live MO
    /// delivery end-to-end through a real framework.
    ///   cd target/canary && curl -LO https://unpkg.com/@hotwired/stimulus@3/dist/stimulus.umd.js
    #[test]
    #[ignore = "manual canary: needs target/canary/stimulus.umd.js"]
    fn stimulus_mutation_observer_canary() {
        let Ok(stimulus) = std::fs::read("target/canary/stimulus.umd.js") else {
            eprintln!("no target/canary/stimulus.umd.js");
            return;
        };
        let html = "<html><head><script src='/stimulus.js'></script></head>\
            <body><div id='host'></div><script>\
            var app = Stimulus.Application.start();\
            app.register('hello', class extends Stimulus.Controller {\
              connect() { this.element.textContent = 'STIMULUS-CONNECTED'; }\
            });\
            setTimeout(function () {\
              var d = document.createElement('div');\
              d.setAttribute('data-controller', 'hello');\
              document.getElementById('host').appendChild(d);\
            }, 10);\
            </script></body></html>";
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![("/stimulus.js".to_string(), Some(stimulus))];
        let (out, outcome) = transform(html, &env);
        eprintln!("stimulus outcome: {outcome:?}");
        assert!(!outcome.panicked, "Stimulus panicked the engine");
        assert!(
            out.contains("STIMULUS-CONNECTED"),
            "Stimulus's MutationObserver must connect a dynamically-added controller: {out}"
        );
    }

    /// Alpine uses a MutationObserver to initialize `x-data` components added
    /// after start. We inject the component only after `alpine:initialized`
    /// (observer live), so it is Alpine's MutationObserver — not its initial
    /// tree walk — that runs the new node's `x-init`.
    ///   cd target/canary && curl -L -o alpine.min.js https://unpkg.com/alpinejs@3/dist/cdn.min.js
    #[test]
    #[ignore = "manual canary: needs target/canary/alpine.min.js"]
    fn alpine_mutation_observer_canary() {
        let Ok(alpine) = std::fs::read("target/canary/alpine.min.js") else {
            eprintln!("no target/canary/alpine.min.js");
            return;
        };
        // Alpine's CDN auto-start uses a readiness helper that doesn't fire in
        // our load sequence, so start it explicitly (the documented manual
        // path; Alpine guards against a double start). By `alpine:initialized`
        // its MutationObserver is live, so the node injected then is initialized
        // by that observer — exactly what we're proving.
        let html = "<html><head><script src='/alpine.js'></script></head>\
            <body><div id='host'></div><script>\
            document.addEventListener('alpine:initialized', function () {\
              var d = document.createElement('div');\
              d.setAttribute('x-data', '{}');\
              d.setAttribute('x-init', \"$el.textContent = 'ALPINE-INIT'\");\
              document.getElementById('host').appendChild(d);\
            });\
            window.Alpine.start();\
            </script></body></html>";
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![("/alpine.js".to_string(), Some(alpine))];
        let (out, outcome) = transform(html, &env);
        eprintln!("alpine outcome: {outcome:?}");
        assert!(!outcome.panicked, "Alpine panicked the engine");
        assert!(
            out.contains("ALPINE-INIT"),
            "Alpine's MutationObserver must initialize a dynamically-added x-data node: {out}"
        );
    }

    /// htmx 2.x uses NO MutationObserver (it processes content via
    /// `htmx.process()` / swaps), so it was never an MO proof — it was vendored
    /// alongside the MO canaries as a common-framework boot check. The honest
    /// finding: htmx is CURRENTLY BLOCKED at module init by a missing
    /// `XPathEvaluator` global — it does
    /// `(new XPathEvaluator).createExpression('.//*[@*[starts-with(name(),"hx-")…]]')`
    /// to find `hx-*` elements (CSS can't match an attribute-name prefix).
    /// Unblocking htmx needs a real XPath engine — a separate compat task, NOT
    /// part of MutationObserver. What this canary DOES pin is the robustness
    /// property that matters here: a missing platform global makes htmx DEGRADE
    /// GRACEFULLY (the page still renders, the engine does not abort), surfacing
    /// the gap as an `outcome.error` rather than a crash.
    ///   cd target/canary && curl -L -o htmx.min.js https://unpkg.com/htmx.org@2/dist/htmx.min.js
    #[test]
    #[ignore = "manual canary: needs target/canary/htmx.min.js"]
    fn htmx_boot_documents_xpath_gap() {
        let Ok(htmx) = std::fs::read("target/canary/htmx.min.js") else {
            eprintln!("no target/canary/htmx.min.js");
            return;
        };
        let html = "<html><head><script src='/htmx.js'></script></head>\
            <body><div id='present' hx-get='/x'>present</div></body></html>";
        let mut env = PageEnv::bare("https://example.com/");
        env.externals = vec![("/htmx.js".to_string(), Some(htmx))];
        let (out, outcome) = transform(html, &env);
        eprintln!("htmx outcome: {outcome:?}");
        // Graceful degrade: no engine abort, the page content survives.
        assert!(
            !outcome.panicked,
            "a missing global must not abort the engine"
        );
        assert!(out.contains("present"), "the page must still render: {out}");
        // Document the known blocker (a tripwire: if htmx ever boots clean here,
        // this flips and we update the note — htmx still wouldn't use MO).
        assert!(
            outcome.errors.iter().any(|e| e.contains("XPathEvaluator")),
            "expected the XPathEvaluator boot gap; htmx errors were: {:?}",
            outcome.errors
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
    fn image_load_event_fires_and_reveals_hidden_images() {
        // The "reveal on load" idiom: an <img> painted at opacity:0 until a load
        // handler reveals it (lightGallery's lightbox, lazy-loaders, fade-ins).
        // A headless DOM never decodes images, so we fire a synthetic `load`;
        // without it the image stays opacity:0 and the layout/serializer drops
        // it entirely. The handler runs AND the revealed image survives.
        let html = r#"<body><pre id="o"></pre>
            <img id="pic" src="https://example.com/p.jpg" style="opacity:0">
            <script>
              var pic = document.getElementById('pic');
              pic.addEventListener('load', function () {
                pic.style.opacity = '1';
                document.getElementById('o').textContent = 'loaded';
              });
            </script></body>"#;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("loaded"), "the load handler fired: {out}");
        assert!(
            out.contains(r#"id="pic""#),
            "the revealed image survives serialization (no longer opacity:0): {out}"
        );
    }

    #[test]
    fn an_image_with_no_load_listener_is_left_alone() {
        // The scan only fires `load` where something is waiting on it: an
        // ordinary image (no load handler) is untouched — no needless events on
        // image-heavy pages.
        let html = r#"<body><pre id="o">start</pre>
            <img id="pic" src="https://example.com/p.jpg">
            <script>
              // No load listener bound; if a spurious load fired and bubbled to
              // window it would not change this, but assert the page is intact.
              document.getElementById('o').textContent = 'ok';
            </script></body>"#;
        let (out, outcome) = transform(html, &PageEnv::bare("https://example.com/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("ok") && out.contains(r#"id="pic""#), "{out}");
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
    fn a_click_driven_animation_completes_within_the_dispatch() {
        // Humble's "Dismiss banner" calls jQuery `.slideUp({duration:500, done:
        // remove})`: it steps an animation off `Date.now()` and removes the
        // element only on completion. The dispatch settle advances VIRTUAL time
        // (1000ms a tick), so the clocks must track it — a wall-clock
        // `Date.now()` barely moves in that microsecond burst, so the animation
        // saw ~0 elapsed and the banner never went away. Here a click starts a
        // `Date.now()`/`requestAnimationFrame` animation that removes the
        // element after 500ms; it must finish inside the one dispatch.
        // The handler is DELEGATED on an ancestor and matched by selector (how
        // Backbone — Humble's framework — binds `click .js-dismiss-button`), so
        // this also exercises that the synthetic click BUBBLES to the delegate.
        let (handle, mut events) = live(
            r##"<body><div id="banner">BANNER</div><button class="js-dismiss-button">close</button><script>
            document.addEventListener('click', function (e) {
                if (!(e.target.closest && e.target.closest('.js-dismiss-button'))) return;
                var el = document.getElementById('banner');
                var start = Date.now(), dur = 500;
                (function step() {
                    if (Date.now() - start >= dur) { el.parentNode.removeChild(el); return; }
                    requestAnimationFrame(step);
                })();
            });
            </script></body>"##,
        );
        let mut rendered = String::new();
        for _ in 0..4 {
            match events.blocking_recv() {
                Some(PageEvt::Updated { html, .. }) | Some(PageEvt::Static { html, .. }) => {
                    rendered = html;
                    if rendered.contains("BANNER") {
                        break;
                    }
                }
                other => panic!("expected a render, got {other:?}"),
            }
        }
        assert!(rendered.contains("BANNER"), "initial render: {rendered}");
        let button = rendered
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .expect("a clickable marker for the button")
            .parse::<usize>()
            .unwrap();
        handle.cmds.blocking_send(PageCmd::Click(button)).unwrap();
        let Some(PageEvt::Updated { html, outcome }) = events.blocking_recv() else {
            panic!("expected Updated after the click");
        };
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            !html.contains("BANNER"),
            "the 500ms click-driven animation must complete and remove the element: {html}"
        );
    }

    #[test]
    fn timers_keep_firing_after_virtual_time_passes_the_tick_window() {
        // `__trust.tick(1000)` takes a WINDOW from the current virtual time, not
        // an absolute 1000ms cap. The old code compared each timer's absolute
        // deadline against a constant 1000, so once virtual time crept past
        // ~1000ms (a few rAF frames into the page) NO positive-delay timer could
        // fire again — every new `at` was `now + delay > 1000`. That silently
        // froze all timer-driven work, most visibly a socket-streamed reply
        // whose framework defers its render via rAF (Open WebUI rendered an
        // empty message body). Here a load-time rAF counter must run 120 frames
        // (~1920ms of virtual time — well past the old 1000ms wall) to set a
        // marker; with the bug it stalls around frame 62 (now≈1000) and the
        // marker never appears.
        let (_handle, mut events) = live(
            r##"<body><div id="n">0</div><script>
            var n = 0;
            (function step() {
                n++;
                document.getElementById('n').textContent = String(n);
                if (n >= 120) { document.body.setAttribute('data-raf-done', 'yes'); return; }
                requestAnimationFrame(step);
            })();
            </script></body>"##,
        );
        let mut rendered = String::new();
        for _ in 0..4 {
            match events.blocking_recv() {
                Some(PageEvt::Updated { html, .. }) | Some(PageEvt::Static { html, .. }) => {
                    rendered = html;
                    break;
                }
                Some(PageEvt::Settled) => continue,
                other => panic!("expected a render, got {other:?}"),
            }
        }
        assert!(
            rendered.contains("data-raf-done=\"yes\""),
            "the rAF chain must keep firing past 1000ms of virtual time: {rendered}"
        );
        assert!(
            rendered.contains(">120<"),
            "the counter should have reached 120: {rendered}"
        );
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

    #[test]
    fn contenteditable_host_is_an_editable_field() {
        // A `contenteditable` div is a typeable field. The host keeps the page
        // resident (like a form control); SetValue drives the real editing
        // algorithm — a cancelable `beforeinput`, then the content is replaced
        // and `input` fires (inputType "insertText") — so a listener sees the
        // edit and the host's text reflects it. This is the general
        // rich-text-editor input path (ProseMirror/TipTap, Quill, comment boxes).
        let html = "<body>\
             <div contenteditable=\"true\" id=\"ed\" data-placeholder=\"Type here\"></div>\
             <pre id=\"out\"></pre>\
             <script>\
               const ed = document.getElementById('ed');\
               ed.addEventListener('input', (e) => { \
                 document.getElementById('out').textContent = 'in:' + ed.textContent + '|it:' + e.inputType; });\
             </script></body>";
        let env = PageEnv::bare("https://example.com/");
        let (handle, mut events) = spawn_page(html.to_string(), env);
        // First render: the live shell. The contenteditable host keeps it live.
        let first = match events.blocking_recv() {
            Some(PageEvt::Updated { html, outcome }) => {
                assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                html
            }
            other => panic!("expected a live Updated shell, got {other:?}"),
        };
        // The host carries a data-trust-node (it routes to the editable path).
        let node = first
            .split("data-trust-node=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .expect("a data-trust-node on the contenteditable host")
            .parse::<usize>()
            .unwrap();
        handle
            .cmds
            .blocking_send(PageCmd::SetValue {
                node,
                value: "hello world".into(),
                checked: None,
            })
            .unwrap();
        let Some(PageEvt::Updated { html, outcome }) = events.blocking_recv() else {
            panic!("expected Updated after editing the contenteditable");
        };
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            html.contains("in:hello world|it:insertText"),
            "input listener did not see the edit: {html}"
        );
        assert!(
            html.contains("hello world"),
            "host content not updated: {html}"
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

    #[test]
    fn clicking_a_submit_button_fires_the_form_submit() {
        // A live <button type=submit> reaches the app as a JsClick (the live
        // serializer wraps every button). Clicking it must run the form-
        // submission algorithm so the form's own `submit` handler (React's
        // onSubmit is bound on the <form>, not the button) fires — pixiv's
        // login button did "nothing" because only a bare click event went out.
        let (handle, mut events) = live(
            "<body><form id=f><input name=q><button type=submit>Go</button></form>\
             <pre id=out>idle</pre><script>\
             document.getElementById('f').addEventListener('submit', function (e) {\
                 e.preventDefault();\
                 document.getElementById('out').textContent = 'submitted:' + (e.submitter ? e.submitter.textContent : 'none');\
             });</script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("live form should keep the actor resident");
        };
        let button = live_node_after(&html, "<button");
        handle.cmds.blocking_send(PageCmd::Click(button)).unwrap();
        // Submit was prevented → the page owns the update; the handler ran and
        // saw the submitter, and the app is NOT asked to navigate.
        let html2 = loop {
            match events.blocking_recv() {
                Some(PageEvt::Updated { html, .. }) => break html,
                Some(PageEvt::Settled) => continue,
                other => panic!("expected the prevented submit to re-render, got {other:?}"),
            }
        };
        assert!(html2.contains("submitted:Go"), "{html2}");
    }

    #[test]
    fn clicking_a_submit_button_without_a_handler_asks_the_app_to_submit() {
        // A LIVE page (has JS) whose <form> has NO `submit` handler: a click on
        // its submit button fires an un-prevented submit, so the app runs the
        // native GET/POST (the event carries the form + submitter nodes). The
        // button's own click listener doesn't preventDefault — it just makes
        // the page live so the button reaches the app as a JsClick.
        let (handle, mut events) = live(
            "<body><form action=\"/go\"><input name=q value=hi>\
             <button type=submit>Go</button></form>\
             <script>document.querySelector('button').addEventListener('click', function () {});</script></body>",
        );
        let Some(PageEvt::Updated { html, .. }) = events.blocking_recv() else {
            panic!("a live form keeps the actor resident");
        };
        let at = html.find("Go").expect("button label");
        let button = html[..at]
            .rfind("x-trust-js:")
            .map(|i| {
                html[i + "x-trust-js:".len()..]
                    .split(':')
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
            .expect("button marker");
        handle.cmds.blocking_send(PageCmd::Click(button)).unwrap();
        match events.blocking_recv() {
            Some(PageEvt::SubmitForm { .. }) => {}
            other => {
                panic!("unprevented submit-button click should ask app to submit, got {other:?}")
            }
        }
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
    fn inline_cache_revalidates_prototype_shape_on_mutation() {
        // Boa fork fix: the monomorphic property inline cache guarded only the
        // RECEIVER shape. For a direct-prototype hit the cached slot indexes the
        // PROTOTYPE's own storage, so mutating the prototype (delete/redefine)
        // shifts those slots while the receiver shape is untouched — leaving the
        // cached index stale. Before the fix case (1) ABORTED the VM with an
        // out-of-bounds (len 1, index 1) and case (2) could return a silently
        // WRONG value. The fork now also pins the prototype-holder shape and
        // re-validates it on every hit.
        let (out, outcome) = page(
            "<body><pre id=out></pre><script>\
             var log = [];\
             (function () {\
               var proto = { a: 1, b: 2 };\
               var o = Object.create(proto);\
               function read() { return o.b; }\
               read(); read();           /* warm the cache (prototype hit) */\
               delete proto.a;           /* b shifts from slot 1 -> slot 0 */\
               log.push('b=' + read());  /* must be 2, must not panic */\
             })();\
             (function () {\
               var proto = { x: 10, y: 20 };\
               var o = Object.create(proto);\
               function readY() { return o.y; }\
               readY(); readY();\
               delete proto.x;           /* y -> slot 0 */\
               proto.z = 99;             /* z reuses the old slot 1 */\
               log.push('y=' + readY()); /* must still be 20, not 99 */\
             })();\
             document.getElementById('out').textContent = log.join(' ');\
             </script></body>",
        );
        assert!(
            !outcome.panicked,
            "engine panicked (inline-cache prototype OOB regressed)"
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("b=2"),
            "stale prototype slot (OOB case): {out}"
        );
        assert!(
            out.contains("y=20"),
            "stale prototype slot (wrong-value case): {out}"
        );
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
    fn define_catch_up_upgrades_every_existing_instance_in_order() {
        // define()'s catch-up upgrade is now backed by a Rust composed-tree
        // walk (__dom_upgrade_candidates), not a JS per-node recursion — it
        // must still find EVERY pre-existing instance (GitHub SSRs many of one
        // custom element) and upgrade them in document order, including one
        // inside a shadow root. Each connectedCallback appends its data-n to a
        // shared log; the log proves count, completeness, and order.
        let (out, outcome) = page(
            "<body><div id=log></div>\
             <my-el data-n=1></my-el><my-el data-n=2></my-el>\
             <div id=host></div>\
             <my-el data-n=4></my-el><script>\
             const host = document.getElementById('host');\
             const sr = host.attachShadow({ mode: 'open' });\
             sr.innerHTML = '<my-el data-n=3></my-el>';\
             class MyEl extends HTMLElement {\
               connectedCallback() {\
                 const l = document.getElementById('log');\
                 l.textContent = l.textContent + this.getAttribute('data-n');\
               }\
             }\
             customElements.define('my-el', MyEl);\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // All four instances upgraded+connected, in document (pre-)order — the
        // shadow instance (#3) sits at its host's tree position.
        assert!(out.contains(">1234<"), "expected log 1234, got: {out}");
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
    fn named_node_map_supports_named_property_access() {
        // A real NamedNodeMap exposes each attribute by name as well as by
        // index (WebIDL named getter): `el.attributes["title"]` returns the
        // Attr. jQuery's event-support probe reads
        // `div.attributes[eventName].expando` after setAttribute; without the
        // named getter that was `undefined.expando` → a ToObject throw that
        // aborted jQuery's boot on humblebundle.com.
        let (out, outcome) = page(
            "<body><div id=root title=hi></div><p id=o></p><script>\
             const d = document.getElementById('root');\
             d.setAttribute('onsubmit', 't');\
             const named = d.attributes['title'];\
             const probe = false === d.attributes['onsubmit'].expando;\
             document.getElementById('o').textContent =\
               [named.value, named.name, probe, String(d.attributes['nope'])].join('|');\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // named['title'] resolves to its Attr; the jQuery probe runs without
        // throwing; a missing name is undefined (not an error).
        assert!(out.contains(">hi|title|false|undefined<"), "{out}");
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
            // NumberFormat/DateTimeFormat/Collator are specced callable WITHOUT
            // `new` (Humble Bundle does `Intl.NumberFormat(loc,opts).format(x)`);
            // instanceof and the prototype methods must still work either way.
            ok.push(Intl.NumberFormat("en", { style: "currency", currency: "EUR" }).format(12.99) === "€12.99");
            ok.push(Intl.NumberFormat() instanceof Intl.NumberFormat);
            ok.push(Intl.DateTimeFormat(0, { year: "numeric", month: "numeric", day: "numeric" })
                .format(new Date(2026, 5, 12)) === "2026-06-12");
            ok.push(typeof Intl.Collator("en").compare === "function");
            document.getElementById('out').textContent = ok.join(' ');
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains(
                "true true true true true true true true true true true true true true true"
            ),
            "Intl probes failed: {out}"
        );
    }

    #[test]
    fn date_parses_microsecond_timestamps_like_a_browser() {
        // Real-world servers emit sub-millisecond ISO timestamps (Python
        // `isoformat`, databases): `…02.200128Z` (microseconds). Browsers
        // accept any number of fractional digits, truncating to milliseconds;
        // Boa rejected anything but exactly 3 → an Invalid Date that threw in
        // humblebundle.com's bundle-page DOMContentLoaded handler. Fixed in the
        // Boa fork (lenient fractional-seconds parse).
        let (out, outcome) = page(
            "<body><p id=o></p><script>\
             const ms = Date.parse('2026-06-20T11:52:02.200128Z');\
             const ns = new Date('2026-06-20T11:52:02.200128456Z');\
             document.getElementById('o').textContent =\
               [!isNaN(ms), new Date(ms).getUTCMilliseconds(), ns.toISOString()].join('|');\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains(">true|200|2026-06-20T11:52:02.200Z<"), "{out}");
    }

    #[test]
    fn element_geometry_reports_real_cell_boxes() {
        // getBoundingClientRect / offset* now return the element's REAL laid-out
        // box (a layout pass over the live DOM), not the viewport fiction.
        // "HELLO" is 5 cells; cell_px is 8x16 → 40px wide, 16px tall. The
        // viewport is 80x24 cells (640x384px), so a real box (40x16) is
        // unmistakably distinct from the old fallback (640x384).
        let (out, outcome) = page(
            r##"<body><div id=probe>HELLO</div><div id=out></div><script>
            var p = document.getElementById('probe');
            var r = p.getBoundingClientRect();
            document.getElementById('out').textContent =
                [r.left, r.width, r.height, p.offsetWidth, p.offsetHeight].join(' ');
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("0 40 16 40 16"),
            "geometry should be the real cell box, got: {out}"
        );
    }

    #[test]
    fn computed_style_reports_ua_default_display() {
        // getComputedStyle(el).display reports the HTML UA stylesheet default
        // for an element with no author `display` (block/inline/list-item/…).
        // jQuery's `.show()` reads this to learn an element's default display;
        // an empty value sent it down a temp-`<iframe>` probe
        // (`iframe.contentWindow.document`) the prelude can't satisfy, throwing
        // and tearing down Marionette's render path on humblebundle.com. An
        // author rule still wins over the UA default.
        let (out, outcome) = page(
            "<head><style>#styled{display:flex}</style></head>\
             <body><div id=d>x</div><span id=s>y</span><li id=l>z</li>\
             <div id=styled>w</div><p id=o></p><script>\
             const g = (id) => getComputedStyle(document.getElementById(id)).display;\
             document.getElementById('o').textContent =\
               [g('d'), g('s'), g('l'), g('styled')].join('|');\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains(">block|inline|list-item|flex<"), "{out}");
    }

    #[test]
    fn reflected_string_attributes_return_empty_not_undefined() {
        // HTML reflected string IDL attributes (lang/dir/title/slot) must return
        // the content attribute or "" — NEVER undefined. pixiv boots with
        // `document.documentElement.lang.toLowerCase()`; an undefined `lang`
        // made `.toLowerCase()` throw "cannot convert undefined to object"
        // (Boa's wording for a property read off undefined), killing its whole
        // bundle before `window.pixiv` was defined.
        let (out, outcome) = page(
            "<html lang=\"JA\"><head></head><body dir=rtl title=hi>\
             <div id=o></div><script>\
             const h = document.documentElement;\
             document.getElementById('o').textContent = [\
               h.lang.toLowerCase(), typeof h.dir, document.body.dir,\
               document.body.title, typeof document.body.slot,\
             ].join('|');\
             document.documentElement.lang = 'en';\
             document.getElementById('o').setAttribute('data-set', document.documentElement.getAttribute('lang'));\
             </script></body></html>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // lang reflects (lowercased by the caller), absent attrs are "" (string).
        assert!(out.contains(">ja|string|rtl|hi|string<"), "{out}");
        // The setter reflects back to the content attribute.
        assert!(out.contains("data-set=\"en\""), "{out}");
    }

    #[test]
    fn meta_content_reflects_its_attribute_for_json_boot_config() {
        // HTMLMetaElement.content reflects the `content` attribute. pixiv stashes
        // its boot config as JSON in `<meta id=meta-global-data content='{…}'>`
        // and does `JSON.parse(meta.content)`; an expando-`undefined` content
        // made that a SyntaxError. A non-<meta> element still gets a plain
        // `.content` expando (lit's `.content=` property binding) and <template>
        // keeps its fragment.
        let (out, outcome) = page(
            "<head><meta id=cfg content='{\"token\":\"abc\",\"n\":1}'></head>\
             <body><div id=o></div><div id=lit></div><script>\
             const cfg = JSON.parse(document.getElementById('cfg').content);\
             document.getElementById('lit').content = 42;\
             document.getElementById('o').textContent =\
               cfg.token + '|' + cfg.n + '|' + document.getElementById('lit').content;\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains(">abc|1|42<"), "{out}");
    }

    #[test]
    fn iframe_content_document_and_window_are_a_real_same_arena_document() {
        // `iframe.contentDocument || iframe.contentWindow.document` is the
        // universal frame-access idiom (analytics beacons, the Cloudflare
        // JS-detection script pixiv embeds). We hand back a real same-arena
        // FrameDocument: the idiom doesn't throw, `contentWindow.document` is
        // the SAME object, and a fresh frame has the `<html><head><body>`
        // skeleton. Content written into it renders inline (covered by
        // `iframe_scripted_document_write_renders_inline`); a non-frame element
        // has no such property (undefined).
        let (out, outcome) = page(
            r##"<body><div id=o></div><script>
            var f = document.createElement('iframe');
            document.body.appendChild(f);
            // The exact CF beacon shape: contentDocument first, else window.document.
            var d = f.contentDocument || f.contentWindow.document;
            var s = d.createElement('script');
            d.getElementsByTagName('head')[0].appendChild(s);
            var div = document.createElement('div');
            document.getElementById('o').textContent = [
              typeof d, d === f.contentWindow.document,
              d.getElementsByTagName('head').length, typeof div.contentDocument,
            ].join('|');
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // a document facade, window.document is the SAME object, head exists,
        // and a plain <div> has no contentDocument.
        assert!(out.contains(">object|true|1|undefined<"), "{out}");
    }

    #[test]
    fn iframe_scripted_document_write_renders_inline() {
        // The W3Schools tryit-editor pattern (and every code-playground result
        // pane): create an iframe, append it, then `contentWindow.document
        // .open()/write()/close()` a whole HTML document into it. The nested
        // body must flow into the page so the written content actually renders
        // (here, a table) rather than being inert.
        let (out, outcome) = page(
            r##"<body><div id=wrap></div><script>
            var ifr = document.createElement('iframe');
            document.getElementById('wrap').appendChild(ifr);
            var w = ifr.contentWindow;
            w.document.open();
            w.document.write('<!DOCTYPE html><html><head><style>td{border:1px solid}</style></head><body><h1>A Fancy Table</h1><table><tr><th>Company</th><th>Country</th></tr><tr><td>Alfreds</td><td>Germany</td></tr></table></body></html>');
            w.document.close();
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("data-trust-frame"),
            "frame wrapper missing: {out}"
        );
        assert!(
            out.contains("A Fancy Table"),
            "frame heading missing: {out}"
        );
        assert!(
            out.contains("Alfreds") && out.contains("Germany"),
            "table cells missing: {out}"
        );
        // The frame's own <style> is dropped by the serializer (baked already),
        // and the content is a normal block (no <iframe> RAWTEXT in the output).
        assert!(
            !out.contains("<iframe"),
            "iframe element should not survive: {out}"
        );
    }

    #[test]
    fn iframe_srcdoc_renders_inline() {
        // A declarative `srcdoc` frame renders its content even when no script
        // touches it (`hydrateFrames` realizes it at DOMContentLoaded).
        let (out, outcome) =
            page(r##"<body><iframe srcdoc="<p>SANDBOXED CONTENT</p>"></iframe></body>"##);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("SANDBOXED CONTENT"),
            "srcdoc content missing: {out}"
        );
    }

    #[test]
    fn intersection_observer_reports_the_targets_real_box() {
        // The observer fires for every target (the no-scroll terminal safeguard)
        // with the element's REAL box for `boundingClientRect` and a FULL
        // intersection (ratio 1) — we render the whole document, so every
        // observed element is treated as fully visible.
        let (out, outcome) = page(
            r##"<body><div id=probe>HELLO</div><div id=out></div><script>
            var io = new IntersectionObserver(function (entries) {
                var e = entries[0];
                document.getElementById('out').textContent =
                    [e.isIntersecting, e.boundingClientRect.width, e.intersectionRatio].join(' ');
            });
            io.observe(document.getElementById('probe'));
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("true 40 1"),
            "IO record should carry the real box + full ratio, got: {out}"
        );
    }

    #[test]
    fn intersection_observer_loads_below_fold_lazy_images() {
        // vanilla-lazyload (and others) gate on `intersectionRatio > 0` and
        // IGNORE `isIntersecting`. A below-the-fold element's real ratio is 0,
        // so such a lazy-loader left every below-fold image blank — Humble
        // Bundle's bundle-item grid loads only the top rows. Since we render the
        // whole document, an observed element reports ratio 1 regardless of its
        // box position, so the loader materializes all of them. The probe sits
        // far below the viewport, yet its ratio is still positive.
        let (out, outcome) = page(
            r##"<body><div style="height:5000px"></div>
            <img id=lazy data-lazy="/x.png"><div id=out></div><script>
            var io = new IntersectionObserver(function (entries) {
                entries.forEach(function (e) {
                    // The vanilla-lazyload condition, verbatim.
                    if (e.intersectionRatio > 0) {
                        e.target.src = e.target.getAttribute('data-lazy');
                        io.unobserve(e.target);
                    }
                });
            });
            io.observe(document.getElementById('lazy'));
            window.__after = function () {
                document.getElementById('out').textContent =
                    'src=' + (document.getElementById('lazy').getAttribute('src') || 'NONE');
            };
            setTimeout(window.__after, 0);
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("src=/x.png"),
            "a below-fold lazy image must still load, got: {out}"
        );
    }

    // --- MutationObserver -------------------------------------------------

    #[test]
    fn mutation_observer_reports_childlist_add_and_remove() {
        // Two synchronous mutations COALESCE into one microtask delivery with
        // two records — the spec's compound-microtask batching.
        let (out, outcome) = page(
            r##"<body><div id="t"></div><pre id="out"></pre><script>
            var out = document.getElementById('out'), t = document.getElementById('t');
            var log = [];
            new MutationObserver(function (recs) {
                for (var i = 0; i < recs.length; i++) {
                    var r = recs[i];
                    log.push(r.type + ':+' + r.addedNodes.length + '-' + r.removedNodes.length);
                }
                out.textContent = log.join(' ');
            }).observe(t, { childList: true });
            var span = document.createElement('span');
            t.appendChild(span);
            t.removeChild(span);
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("childList:+1-0 childList:+0-1"), "{out}");
    }

    #[test]
    fn mutation_observer_reports_attributes_with_old_value_and_filter() {
        let (out, outcome) = page(
            r##"<body><div id="t" class="a" data-x="1"></div><pre id="out"></pre><script>
            var out = document.getElementById('out'), t = document.getElementById('t');
            var log = [];
            new MutationObserver(function (recs) {
                for (var r of recs) log.push(r.attributeName + '=' + r.oldValue);
                out.textContent = log.join(' ');
            }).observe(t, { attributes: true, attributeOldValue: true, attributeFilter: ['class'] });
            t.setAttribute('class', 'b');   // observed (filter includes class)
            t.setAttribute('data-x', '2');  // filtered out
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // The observer's report is exactly the one filtered-in attribute: the
        // <pre> holds "class=a" (old value of class) and nothing about data-x.
        // (The div itself still carries data-x="2" in the serialized markup —
        // assert on the <pre> content, not the whole document.)
        assert!(
            out.contains(">class=a</pre>"),
            "filtered report should be only class=a: {out}"
        );
    }

    #[test]
    fn mutation_observer_reports_character_data_with_old_value() {
        let (out, outcome) = page(
            r##"<body><p id="t">hello</p><pre id="out"></pre><script>
            var out = document.getElementById('out');
            var t = document.getElementById('t').firstChild;  // the text node
            new MutationObserver(function (recs) {
                out.textContent = recs[0].type + ':' + recs[0].oldValue;
            }).observe(t, { characterData: true, characterDataOldValue: true });
            t.data = 'world';
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("characterData:hello"), "{out}");
    }

    #[test]
    fn mutation_observer_subtree_matches_deep_but_non_subtree_does_not() {
        // `subtree:true` sees a mutation on a deep descendant (via the
        // __dom_contains ancestor syscall); a plain registration on the same
        // root sees only its direct children.
        let (out, outcome) = page(
            r##"<body><div id="root"><div id="mid"><span id="leaf"></span></div></div>
            <pre id="a"></pre><pre id="b"></pre><script>
            var root = document.getElementById('root'), leaf = document.getElementById('leaf');
            var na = 0, nb = 0;
            new MutationObserver(function (r) { na += r.length; document.getElementById('a').textContent = 'a=' + na; })
                .observe(root, { childList: true, subtree: true });
            new MutationObserver(function (r) { nb += r.length; document.getElementById('b').textContent = 'b=' + nb; })
                .observe(root, { childList: true });   // no subtree
            leaf.appendChild(document.createElement('b'));  // deep descendant mutation
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("a=1"),
            "subtree observer must see the deep mutation: {out}"
        );
        assert!(
            !out.contains("b=1"),
            "non-subtree observer must NOT see a descendant mutation: {out}"
        );
    }

    #[test]
    fn mutation_observer_disconnect_and_take_records() {
        let (out, outcome) = page(
            r##"<body><div id="t"></div><pre id="out"></pre><script>
            var out = document.getElementById('out'), t = document.getElementById('t');
            var fired = 0;
            var mo = new MutationObserver(function () { fired++; });
            mo.observe(t, { childList: true });
            t.appendChild(document.createElement('a'));   // queues a record
            var pending = mo.takeRecords().length;        // drains it synchronously
            mo.disconnect();
            t.appendChild(document.createElement('b'));   // ignored after disconnect
            out.textContent = 'pending=' + pending + ' fired=' + fired;
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // takeRecords drained the queued record, so the async callback saw
        // nothing; the post-disconnect mutation never queued.
        assert!(out.contains("pending=1 fired=0"), "{out}");
    }

    #[test]
    fn mutation_observer_delivers_as_microtask_before_timers() {
        // Ordering proof: synchronous code first; microtasks (a promise
        // reaction queued before the mutation, then the MO delivery) run before
        // the macrotask timer. This is the async-XHR-vs-promise ordering trap's
        // cousin — getting it wrong reorders observer callbacks against fetch.
        let (out, outcome) = page(
            r##"<body><div id="t"></div><pre id="out"></pre><script>
            var seq = [], t = document.getElementById('t');
            function finish() { document.getElementById('out').textContent = seq.join(','); }
            new MutationObserver(function () { seq.push('mo'); finish(); }).observe(t, { childList: true });
            Promise.resolve().then(function () { seq.push('promise'); finish(); });
            setTimeout(function () { seq.push('timeout'); finish(); }, 0);
            t.appendChild(document.createElement('x'));   // queues the MO microtask
            seq.push('sync');
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("sync,promise,mo,timeout"), "ordering: {out}");
    }

    #[test]
    fn mutation_observer_callback_mutation_requeues_then_settles() {
        // A callback that itself mutates the observed tree re-queues a delivery
        // (legal, spec) and the chain terminates once it stops mutating —
        // proving the reset-on-quiescence loop guard doesn't false-trip.
        let (out, outcome) = page(
            r##"<body><div id="t"></div><pre id="out"></pre><script>
            var t = document.getElementById('t'), turns = 0;
            new MutationObserver(function () {
                turns++;
                if (turns < 3) t.appendChild(document.createElement('i'));
                document.getElementById('out').textContent = 'turns=' + turns;
            }).observe(t, { childList: true });
            t.appendChild(document.createElement('i'));   // turn 1
            </script></body>"##,
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("turns=3"), "{out}");
    }

    #[test]
    fn mutation_observer_runaway_loop_is_capped_and_degrades() {
        // An observer that mutates the observed node on EVERY delivery feeds
        // itself forever; the microtask-checkpoint lid trips and disables
        // delivery for the page (degrade, not hang).
        let (_out, outcome) = page(
            r##"<body><div id="t"></div><script>
            var t = document.getElementById('t');
            new MutationObserver(function () { t.setAttribute('data-n', String(Math.random())); })
                .observe(t, { attributes: true });
            t.setAttribute('data-n', '0');   // kick off the self-feeding loop
            </script></body>"##,
        );
        assert!(
            outcome
                .errors
                .iter()
                .any(|e| e.contains("MutationObserver") && e.contains("disabled")),
            "runaway observer loop should trip the lid: {:?}",
            outcome.errors
        );
    }

    #[test]
    fn live_click_injection_is_seen_by_a_subtree_observer() {
        // The dynamic-enhancement pattern (Stimulus / Alpine / htmx): a
        // body-rooted subtree observer enhances nodes added by a LATER
        // interaction. The click injects a <span data-enhance>; the observer
        // rewrites its text. Without a live MutationObserver this never fires.
        let (handle, mut events) = live(
            r##"<body><div id="host"></div><button id="go">go</button><script>
            new MutationObserver(function (recs) {
                for (var r of recs) for (var i = 0; i < r.addedNodes.length; i++) {
                    var n = r.addedNodes[i];
                    if (n.nodeType === 1 && n.hasAttribute && n.hasAttribute('data-enhance')) n.textContent = 'ENHANCED';
                }
            }).observe(document.body, { childList: true, subtree: true });
            document.getElementById('go').addEventListener('click', function () {
                var s = document.createElement('span');
                s.setAttribute('data-enhance', '');
                document.getElementById('host').appendChild(s);
            });
            </script></body>"##,
        );
        let mut first = String::new();
        for _ in 0..4 {
            match events.blocking_recv() {
                Some(PageEvt::Updated { html, .. }) | Some(PageEvt::Static { html, .. }) => {
                    first = html;
                    if first.contains(">go<") {
                        break;
                    }
                }
                other => panic!("expected a render, got {other:?}"),
            }
        }
        let go = first
            .find("id=\"go\"")
            .and_then(|at| first[..at].rfind("x-trust-js:"))
            .map(|i| {
                first[i + "x-trust-js:".len()..]
                    .split(':')
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
            .expect("a clickable marker for the button");
        handle.cmds.blocking_send(PageCmd::Click(go)).unwrap();
        let Some(PageEvt::Updated { html, outcome }) = events.blocking_recv() else {
            panic!("expected Updated after the click");
        };
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            html.contains("ENHANCED"),
            "the subtree observer must enhance the click-injected node: {html}"
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
    fn select_options_surface_drives_react_style_option_mounting() {
        // React's <select> commit (postMountWrapper/updateOptions) reads
        // `select.options`, iterates `.length`, and reads/writes each option's
        // `.value`/`.selected`/`.defaultSelected`. A missing
        // `HTMLSelectElement.options` made `select.options` undefined, so
        // `undefined.length` threw a ToObject TypeError that aborted React's
        // whole render — pixiv's login page mounts a language <select>, so the
        // crash left the form a blank shell. Exercise the same surface plus the
        // controlled-select `value` setter, and confirm the selection lands on
        // the `selected` attribute the layout reads.
        let (out, outcome) = page(
            "<body><select id=s>\
               <option value=en>English</option>\
               <option value=ja>\u{65e5}\u{672c}\u{8a9e}</option>\
               <option>plain</option>\
             </select><pre id=o></pre><script>\
             var s = document.getElementById('s');\
             var opts = s.options, n = opts.length, want = 'ja';\
             for (var i = 0; i < n; i++) {\
               var hit = opts[i].value === want;\
               if (opts[i].selected !== hit) opts[i].selected = hit;\
               if (hit) opts[i].defaultSelected = true;\
             }\
             var r = 'len=' + n + ' val=' + s.value + ' idx=' + s.selectedIndex\
               + ' sel=' + s.selectedOptions.length + ' third=' + opts[2].value;\
             s.value = 'en';\
             r += ' after=' + s.selectedIndex + '/' + s.value;\
             document.getElementById('o').textContent = r;\
             </script></body>",
        );
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // .options is iterable+indexable; .value/.selectedIndex/.selectedOptions
        // read the selection; a valueless <option> falls back to its text; the
        // controlled `value=` setter re-points the selection.
        assert!(
            out.contains("len=3 val=ja idx=1 sel=1 third=plain after=0/en"),
            "{out}"
        );
        // The final selection ("en") carries the `selected` attribute the layout
        // and form-submit path read.
        assert!(out.contains("value=\"en\" selected"), "{out}");
    }

    #[test]
    fn an_injected_inline_script_executes() {
        // The universal SDK-loader idiom: page JS builds a <script> and appends
        // it to the document; a real browser runs it. We never did, so a
        // runtime-injected dependency silently never loaded (pixiv's login
        // injects reCAPTCHA this way then polls for it forever). An inline
        // injected script's text now evaluates, mutating the live DOM.
        let (out, outcome) = page(
            "<body><pre id=o>before</pre><script>\
             var s = document.createElement('script');\
             s.textContent = \"document.getElementById('o').textContent = 'injected-ran';\";\
             document.body.appendChild(s);\
             </script></body>",
        );
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("injected-ran"), "{out}");
        // Re-appending the same script must NOT re-run it (HTML "already
        // started" flag), so the text stays as the first run left it.
        let (out2, _) = page(
            "<body><pre id=o>x</pre><script>\
             var c = 0; window.__n = function(){ return ++c; };\
             var s = document.createElement('script');\
             s.textContent = \"document.getElementById('o').textContent = String(window.__n());\";\
             document.body.appendChild(s);\
             document.body.appendChild(s);\
             </script></body>",
        );
        assert!(out2.contains(">1<"), "ran once, not twice: {out2}");
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
    fn document_domain_reports_the_host_and_round_trips() {
        // `document.domain` returns the document's host (GitHub's behaviors
        // bundle throws "Unable to get document domain" without it). The legacy
        // setter is accepted and round-trips, but has no cross-origin effect.
        let (out, outcome) = page(
            "<body><p id=a></p><p id=b></p><script>\
             document.getElementById('a').textContent = document.domain;\
             document.domain = document.domain;\
             document.getElementById('b').textContent = 'set:' + document.domain;\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains(">example.com</p>"), "{out}");
        assert!(out.contains(">set:example.com</p>"), "{out}");
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
    fn external_stylesheets_are_deduplicated() {
        // A page that links the same sheet repeatedly (GitHub references
        // `primer-react-css` six times) must yield it once, so duplicates don't
        // burn the fetch budget and push real sheets — the marketing nav, the
        // page layout — past the cap (which left menus un-collapsed).
        let html = "<head>\
             <link rel=stylesheet href='/primer.css'>\
             <link rel=stylesheet href='/a.css'>\
             <link rel=stylesheet href='/primer.css'>\
             <link rel=stylesheet href='/primer.css'>\
             <link rel=stylesheet href='/b.css'>\
             </head><body></body>";
        assert_eq!(
            external_stylesheets(html),
            vec![
                "/primer.css".to_string(),
                "/a.css".to_string(),
                "/b.css".to_string()
            ]
        );
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
    fn current_script_points_at_the_executing_classic_script() {
        // `document.currentScript` is the classic script element while its own
        // code runs (its parent is the element it sits in), and null once that
        // run is over (here: in the trailing microtask). SvelteKit reads
        // `document.currentScript.parentElement` to find its mount node.
        let (out, outcome) = page(
            "<body><pre id=o></pre><script>\
             var s = document.currentScript;\
             var rec = (s ? s.tagName : 'null') + ',' + (s ? s.parentNode.tagName : '-');\
             Promise.resolve().then(function(){\
               document.getElementById('o').textContent =\
                 rec + ',after=' + (document.currentScript ? 'set' : 'null');\
             });\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("SCRIPT,BODY,after=null"),
            "currentScript during/after: {out}"
        );
    }

    #[test]
    fn import_meta_url_is_the_modules_own_url() {
        // `import.meta.url` is the module's own absolute URL — bundlers resolve
        // sibling chunks with `new URL("./x.js", import.meta.url)`, so a
        // missing url makes every relative resolution throw "Invalid URL".
        let (out, outcome) = page(
            "<body><pre id=o></pre><script type=module>\
             document.getElementById('o').textContent =\
               'meta=' + new URL('./sib.js', import.meta.url).href;\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("meta=https://example.com/a/sib.js"),
            "import.meta.url resolves siblings: {out}"
        );
    }

    #[test]
    fn broadcast_channel_delivers_to_same_name_peers_not_the_sender() {
        // Same-name channels in this page receive each other's messages; the
        // sender does not receive its own. SvelteKit opens one at boot — a
        // missing global was a ReferenceError that aborted the whole mount.
        let (out, outcome) = page(
            "<body><pre id=o></pre><script>\
             var self_got = false, peer = '';\
             var a = new BroadcastChannel('sync');\
             var b = new BroadcastChannel('sync');\
             a.onmessage = function(){ self_got = true; };\
             b.onmessage = function(e){ peer = e.data; };\
             a.postMessage('hello');\
             setTimeout(function(){\
               document.getElementById('o').textContent =\
                 'peer=' + peer + ',self=' + self_got;\
             }, 0);\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("peer=hello,self=false"),
            "BroadcastChannel delivery: {out}"
        );
    }

    #[test]
    fn streams_transform_pipeline_runs() {
        // The Open WebUI shape: subclass TransformStream, pipe a readable
        // THROUGH it, read the transformed output. A missing TransformStream
        // threw a ReferenceError at module-eval (`class X extends undefined`)
        // that 500'd the authenticated route.
        let (out, outcome) = page(
            "<body><pre id=o></pre><script>\
             class Upper extends TransformStream {\
               constructor(){ super({ transform(chunk, c){ c.enqueue(String(chunk).toUpperCase()); } }); }\
             }\
             const rs = new ReadableStream({ start(c){ c.enqueue('ab'); c.enqueue('cd'); c.close(); } });\
             (async () => {\
               const reader = rs.pipeThrough(new Upper()).getReader();\
               let acc = '';\
               for (;;) { const r = await reader.read(); if (r.done) break; acc += r.value; }\
               document.getElementById('o').textContent = 'got=' + acc;\
             })();\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("got=ABCD"), "transform pipeline: {out}");
    }

    #[test]
    fn text_decoder_stream_decodes_piped_bytes() {
        // TextDecoderStream is a TransformStream that decodes Uint8Array
        // chunks to text — the SSE pipeline's first stage.
        let (out, outcome) = page(
            "<body><pre id=o></pre><script>\
             const rs = new ReadableStream({ start(c){ c.enqueue(new Uint8Array([104,105])); c.close(); } });\
             (async () => {\
               const reader = rs.pipeThrough(new TextDecoderStream()).getReader();\
               let acc = '';\
               for (;;) { const r = await reader.read(); if (r.done) break; acc += r.value; }\
               document.getElementById('o').textContent = 'dec=' + acc;\
             })();\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("dec=hi"), "decoder stream: {out}");
    }

    #[test]
    fn element_animate_finishes_so_transition_callbacks_run() {
        // Web Animations API: a terminal animation completes instantly, firing
        // onfinish on a MACROTASK so a caller that assigns onfinish AFTER
        // animate() (Svelte 5's transition system does exactly this) still sees
        // it. Without it `element.animate` was undefined and threw inside
        // Svelte's transition effect, aborting the effect-flush batch — so a
        // sibling effect (a TipTap/ProseMirror editor mount) never ran.
        let (out, outcome) = page(
            "<body><pre id=o></pre><script>\
             const el = document.getElementById('o');\
             const a = el.animate([{opacity:0},{opacity:1}], {duration:200, fill:'forwards'});\
             a.onfinish = () => { el.textContent = 'finished:' + a.playState; };\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("finished:finished"), "animate onfinish: {out}");
    }

    #[test]
    fn svg_element_interfaces_are_defined() {
        // SvelteKit's link handler branches on `e instanceof SVGAElement` — a
        // bare SVGAElement reference was a ReferenceError that broke its link
        // interception. The whole SVG interface zoo is exposed like the HTML one.
        let (out, outcome) = page(
            "<body><pre id=o></pre><script>\
             document.getElementById('o').textContent = 'a=' + (typeof SVGAElement)\
               + ',svg=' + (typeof SVGSVGElement) + ',path=' + (typeof SVGPathElement)\
               + ',base=' + (SVGAElement.prototype instanceof SVGElement);\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("a=function,svg=function,path=function,base=true"),
            "SVG interfaces: {out}"
        );
    }

    #[test]
    fn structured_clone_deep_copies_with_cycles() {
        // `structuredClone` deep-copies the object graph (cycles, nested
        // arrays/objects, Date, Map) — a standard global apps lean on to snapshot
        // state before mutating (Open WebUI's chat submit clones the attachments
        // list; a missing `structuredClone` threw mid-submit so nothing sent).
        let (out, outcome) = page(
            "<body><pre id=o></pre><script>\
             const a = { n: 1, arr: [1, 2, { x: 3 }], d: new Date(0), m: new Map([['k', 'v']]) };\
             a.self = a;\
             const b = structuredClone(a);\
             b.arr[2].x = 99;\
             const ok = (b !== a) && (b.self === b) && (a.arr[2].x === 3)\
               && (b.d instanceof Date && b.d.getTime() === 0) && (b.m.get('k') === 'v');\
             document.getElementById('o').textContent = 'ok=' + ok;\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("ok=true"), "structuredClone: {out}");
    }

    #[test]
    fn response_body_is_a_readable_stream() {
        // A fetch `Response.body` is a real ReadableStream: streaming consumers
        // (SSE/chat completions) read `response.body.getReader()`. It yields the
        // buffered body as one UTF-8 chunk then closes. A null body made
        // `getReader()` throw, so a streamed reply was never read back.
        let (out, outcome) = page(
            "<body><pre id=o></pre><script>\
             (async () => {\
               const r = new Response('hello stream', { status: 200 });\
               const reader = r.body.getReader();\
               const dec = new TextDecoder();\
               let acc = '';\
               for (;;) { const { value, done } = await reader.read(); if (done) break; acc += dec.decode(value); }\
               document.getElementById('o').textContent = 'got=' + acc;\
             })();\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("got=hello stream"),
            "Response.body stream: {out}"
        );
    }

    #[test]
    fn dom_nodes_carry_their_webidl_brand() {
        // Every platform object must report its interface name as
        // @@toStringTag, so `Object.prototype.toString.call(node)` is
        // "[object HTMLDivElement]" / "[object Text]" — not "[object Object]".
        // The is-an-Element idiom `toString.call(x).includes("Element")` (Tippy.js
        // uses exactly this; a miss makes it return [] and then `.destroy()`
        // throws) depends on it. Irregular tags map to their real interface.
        let (out, outcome) = page(
            "<body><p id=o></p><a id=lnk></a><script>\
             const b = (x) => Object.prototype.toString.call(x);\
             const div = document.createElement('div');\
             const btn = document.createElement('button');\
             const p = document.getElementById('o');\
             const a = document.getElementById('lnk');\
             const txt = document.createTextNode('hi');\
             const isEl = (x) => ['Element','Fragment'].some((t)=>b(x).slice(8,-1).indexOf(t)>=0);\
             p.textContent = 'div=' + b(div) + '|btn=' + b(btn) + '|p=' + b(p)\
               + '|a=' + b(a) + '|txt=' + b(txt) + '|isEl=' + isEl(div);\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(
            out.contains("div=[object HTMLDivElement]"),
            "div brand: {out}"
        );
        assert!(
            out.contains("btn=[object HTMLButtonElement]"),
            "btn brand: {out}"
        );
        assert!(
            out.contains("p=[object HTMLParagraphElement]"),
            "p brand: {out}"
        );
        assert!(
            out.contains("a=[object HTMLAnchorElement]"),
            "a brand: {out}"
        );
        assert!(out.contains("txt=[object Text]"), "text brand: {out}");
        assert!(
            out.contains("isEl=true"),
            "tippy-style element check: {out}"
        );
    }

    #[test]
    fn microtask_rejected_promise_with_catch_is_not_unhandled() {
        // A promise that rejects from inside a queued microtask, with
        // `.then().catch()` attached synchronously before it rejects, is
        // HANDLED — the rejection tracker must not report it. This is the
        // i18next/Vite glob-import-miss shape: the bundler returns
        // `new Promise((res, rej) => queueMicrotask(rej.bind(null, err)))`
        // and i18next attaches `.then(...).catch(cb)`. A false "unhandled
        // rejection" here means we diverge from a real browser.
        let (out, outcome) = page(
            "<body><div id=t></div><script>\
             function boom(){ return new Promise(function(res, rej){ \
               queueMicrotask(rej.bind(null, new Error('miss'))); }); }\
             boom().then(function(){}).catch(function(){ \
               document.getElementById('t').textContent = 'caught'; });\
             </script></body>",
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(out.contains("caught"), "catch handler ran: {out}");
        assert!(
            outcome.console.is_empty(),
            "no unhandled rejection: {:?}",
            outcome.console
        );
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
