//! Opt-in Lumen integration probe.
//!
//! This deliberately shares TRust's real platform prelude and integer-handle
//! host boundary while leaving the production Boa page actor untouched. It is
//! the narrow measurement seam used to decide whether a full backend port is
//! justified. Only the three host functions reached by prelude startup and the
//! selected benchmark are present; this is not a page-compatibility backend.

use crate::dom::{Dom, NodeData};
use lumen::bytecode::Tier;
use lumen::embed::{Ctx, EvalError, Value};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

const DEFAULT_URL: &str = "https://example.com/";

struct HostState {
    dom: Rc<RefCell<Dom>>,
    clock: Rc<RealmClock>,
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
    engine.ctx().op_state().put(HostState {
        dom: Rc::new(RefCell::new(Dom::new())),
        clock,
    });
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

fn install_host_boundary(engine: &mut lumen::Engine) {
    // The production Boa realm registers 101 host functions. These three are sufficient to boot
    // the real prelude and run this CPU/event-loop probe, but deliberately do not make Lumen a
    // browser backend. A production port must implement and test the remaining 98 rather than
    // silently stubbing DOM, network, worker, storage, or WebAssembly behavior.
    engine.define_global("__clock_set", 1, host_clock_set);
    engine.define_global("__dom_node_type", 1, host_node_type);
    engine.define_global("__url_parse", 2, host_url_parse);
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
    let id = match args.first() {
        Some(value) => ctx.coerce_number(value)? as usize,
        None => usize::MAX,
    };
    let node_type = ctx
        .host_mut::<HostState>()
        .and_then(|state| {
            let dom = state.dom.borrow();
            dom.is_valid(id).then(|| match &dom.node(id).data {
                NodeData::Element { .. } => 1,
                NodeData::Text(_) => 3,
                NodeData::Comment(_) => 8,
                NodeData::Document => 9,
                NodeData::Doctype => 10,
                NodeData::Fragment => 11,
            })
        })
        .unwrap_or(0);
    Ok(Value::Num(node_type as f64))
}

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
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
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
    Ok(ctx.make_array(parts.into_iter().map(Value::from_string).collect()))
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
        let mut engine = lumen::Engine::new();
        engine.set_tier(Tier::Interp);
        let clock = Rc::new(RealmClock::new());
        let engine_clock = clock.clone();
        engine.set_wall_clock(move || engine_clock.now_ms());
        engine.ctx().op_state().put(HostState {
            dom: Rc::new(RefCell::new(Dom::new())),
            clock,
        });
        install_host_boundary(&mut engine);
        eval(
            &mut engine,
            "globalThis.__trust_cfg = { url: 'https://example.com/' };",
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

    #[test]
    fn tier_names_are_explicit() {
        assert_eq!(parse_tier("interp").unwrap(), Tier::Interp);
        assert_eq!(parse_tier("bytecode").unwrap(), Tier::Bytecode);
        assert_eq!(parse_tier("jit").unwrap(), Tier::Jit);
        assert!(parse_tier("fast").is_err());
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

        for expected in ["v42,micro-42"] {
            let now = eval_value(&mut engine, "__trust.now() + 100", "timer deadline").unwrap();
            assert_eq!(
                call_trust_method(&mut engine, "tickTo", &[now]).as_num_opt(),
                Some(1.0)
            );
            run_microtask_checkpoint(&mut engine);
            assert_eq!(
                string_value(&mut engine, "intervalOrder.join(',')"),
                expected
            );
        }
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
