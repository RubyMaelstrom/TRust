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
    ("__url_parse", 2, host_url_parse),
    ("__url_set", 3, host_url_set),
    ("__dom_attach_shadow", 1, host_attach_shadow),
    ("__dom_shadow_root", 1, host_shadow_root),
    ("__dom_adopt_styles", 2, host_adopt_styles),
    ("__css_parse", 1, host_css_parse),
    ("__css_supports_selector", 1, host_css_supports_selector),
    ("__dom_template_content", 1, host_template_content),
    ("__clock_set", 1, host_clock_set),
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
        assert_eq!(LUMEN_HOST_FUNCTIONS.len(), 47);

        let mut engine = platform_engine();
        for &(name, length, _) in LUMEN_HOST_FUNCTIONS {
            let actual = eval_value(&mut engine, &format!("{name}.length"), name).unwrap();
            assert_eq!(actual.as_num_opt(), Some(length as f64), "{name}.length");
        }
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
