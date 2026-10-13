//! AudioWorkletGlobalScope (Web Audio 1.1, local
//! WebAudio/web-audio-api@2047f16, #AudioWorklet): a TRust JS engine that lives
//! on its BaseAudioContext's rendering thread.
//!
//! The control thread builds a [`Factory`] with what the scope needs of its
//! document (the module fetch settings and the page's MessagePort registry);
//! the rendering thread calls it with the first `addModule()`. The scope is
//! the worker platform plus the worklet layer (`js_audio_worklet.js`), whose
//! control methods this module calls: module evaluation (HTML #fetch-a-
//! worklet-script-graph through the page's module fetch, with destination
//! semantics of a module script), processor construction
//! (#invoking-processor-constructor), and process() once per render quantum.
//! MessagePort messages for the scope wake the rendering thread, which runs
//! them as the context's associated task queue.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use super::message_port_host::Wake;
use super::{
    Ctx, Dom, EvalError, HostState, ModuleFetchEnvironment, RealmClock, Value,
    eval_trust_js_worker_setup, install_trust_js_worker_boundary, module_dependency_loader,
    trust_js_worker_internal_call,
};
use crate::webaudio::bus::Bus;
use crate::webaudio::worklet::{Call, Factory, ProcessorError, Setup, Worklet};

/// The scope's module fetch settings: those of the document whose
/// BaseAudioContext owns it.
struct Launch {
    page: url::Url,
    cookie_context: crate::http::CookieContext,
    handle: tokio::runtime::Handle,
    cache: Arc<crate::http::PageCache>,
    fetched: Arc<std::sync::atomic::AtomicUsize>,
    environment: ModuleFetchEnvironment,
    registry: Arc<std::sync::Mutex<super::message_port_host::Registry>>,
    secure_context: bool,
}

/// The factory for the AudioWorkletGlobalScope of a context created by the
/// calling realm, or `None` without a network (no module can be fetched).
pub(super) fn factory(ctx: &mut Ctx) -> Option<Factory> {
    let context = ctx.host_job_context();
    let page = super::request_context_url(ctx, context);
    let cookie_context = super::cookie_context_for_id(ctx, context);
    let environment = ModuleFetchEnvironment::of(ctx, context);
    let state = ctx.host_mut::<HostState>()?;
    let network = state.network.as_ref()?;
    let launch = Launch {
        secure_context: crate::http::potentially_trustworthy(&page),
        page,
        cookie_context,
        handle: network.handle.clone(),
        cache: network.cache.clone(),
        fetched: network.fetched.clone(),
        environment,
        registry: state.message_ports.registry.clone(),
    };
    Some(Box::new(move |setup: Setup| {
        AudioWorkletScope::new(launch, setup).map(|scope| Box::new(scope) as Box<dyn Worklet>)
    }))
}

fn worklet_prelude_snapshot() -> Result<&'static [u8], String> {
    static SNAPSHOT: std::sync::OnceLock<Result<Vec<u8>, String>> = std::sync::OnceLock::new();
    match SNAPSHOT.get_or_init(|| {
        trust_js::compile_host_snapshot(&crate::js::private_bootstrap(
            crate::js::audio_worklet_prelude(),
        ))
    }) {
        Ok(snapshot) => Ok(snapshot.as_slice()),
        Err(error) => Err(error.clone()),
    }
}

struct AudioWorkletScope {
    engine: trust_js::Engine,
    launch: Launch,
    /// Modules already evaluated (#dom-worklet-addmodule reuses the module
    /// responses map: a module runs once per scope).
    modules: HashSet<String>,
}

impl AudioWorkletScope {
    fn new(launch: Launch, setup: Setup) -> Result<Self, String> {
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock.clone());
        state.base = launch.page.clone();
        state.blobs = launch.environment.blobs.clone();
        state.message_ports.registry = launch.registry.clone();
        let waker = setup.waker.clone();
        state.message_ports.wake = Wake::Callback(Arc::new(move || waker()));
        let mut engine = trust_js::Engine::new();
        let engine_clock = clock.clone();
        engine.set_wall_clock(move || engine_clock.now_ms());
        engine
            .ctx()
            .op_state()
            .put_retained_memory_with_external_memory(state);
        install_trust_js_worker_boundary(&mut engine);
        let config = format!(
            "globalThis.__worker_cfg = {{ id: 0, name: \"\", type: \"module\", url: {}, ua: {}, \
             navigatorMode: {}, platform: {}, language: {}, languages: [{}, {}], hwc: 1, \
             globalPrivacyControl: false, secureContext: {}, timeOrigin: {}, shared: false, \
             worklet: {{ rate: {}, quantum: {} }} }};",
            serde_json::to_string(launch.page.as_str()).unwrap_or_default(),
            serde_json::to_string(crate::http::user_agent()).unwrap_or_default(),
            serde_json::to_string(crate::http::navigator_compatibility_mode().as_str())
                .unwrap_or_default(),
            serde_json::to_string(crate::http::navigator_platform()).unwrap_or_default(),
            serde_json::to_string(crate::locale::LANGUAGE).unwrap_or_default(),
            serde_json::to_string(crate::locale::LANGUAGES[0]).unwrap_or_default(),
            serde_json::to_string(crate::locale::LANGUAGES[1]).unwrap_or_default(),
            launch.secure_context,
            clock.origin_ms,
            setup.rate,
            setup.quantum,
        );
        if !eval_trust_js_worker_setup(&mut engine, &config, "audio worklet configuration")? {
            return Err(String::from("audio worklet setup was interrupted"));
        }
        let snapshot = worklet_prelude_snapshot()?;
        match engine.eval_snapshot_value_interruptible(snapshot) {
            Err(error) => {
                return Err(format!(
                    "audio worklet prelude parse error at line {}: {}",
                    error.line, error.message
                ));
            }
            Ok(Err(EvalError::Throw(error))) => {
                return Err(super::describe_throw(
                    &mut engine,
                    error,
                    "audio worklet prelude",
                ));
            }
            Ok(Err(EvalError::Interrupted(_))) => {
                return Err(String::from("audio worklet setup was interrupted"));
            }
            Ok(Ok(_)) => {}
        }
        Ok(Self {
            engine,
            launch,
            modules: HashSet::new(),
        })
    }

    fn call(&mut self, name: &str, args: &[Value]) -> Result<Value, EvalError> {
        trust_js_worker_internal_call(&mut self.engine, name, args)
    }

    fn error_of(&mut self, text: &str) -> ProcessorError {
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
        ProcessorError {
            message: parsed["message"].as_str().unwrap_or("").to_string(),
            filename: parsed["filename"].as_str().unwrap_or("").to_string(),
            line: parsed["line"].as_u64().unwrap_or(0) as u32,
            column: parsed["column"].as_u64().unwrap_or(0) as u32,
        }
    }

    fn thrown(&mut self, error: EvalError) -> ProcessorError {
        let message = match error {
            EvalError::Throw(value) => {
                super::describe_throw(&mut self.engine, value, "audio worklet")
            }
            EvalError::Interrupted(_) => String::from("the audio worklet was interrupted"),
        };
        ProcessorError {
            message,
            ..Default::default()
        }
    }

    fn element(&mut self, array: &Value, index: usize) -> Option<Value> {
        self.engine.ctx().member_get(array, &index.to_string()).ok()
    }
}

fn float_bytes(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}

impl Worklet for AudioWorkletScope {
    fn add_module(&mut self, url: &str) -> Result<String, String> {
        if self.modules.contains(url) {
            return Ok(String::from("[]"));
        }
        let launch = &self.launch;
        let Some((resolved, source)) = module_dependency_loader(
            &launch.page,
            &launch.cookie_context,
            &launch.handle,
            &launch.cache,
            &launch.fetched,
            None,
            &launch.environment,
            url,
            launch.page.as_str(),
            None,
        ) else {
            return Err(String::from("AbortError:The module could not be fetched"));
        };
        let (page, cookie_context, handle, cache, fetched, environment) = (
            launch.page.clone(),
            launch.cookie_context.clone(),
            launch.handle.clone(),
            launch.cache.clone(),
            launch.fetched.clone(),
            launch.environment.clone(),
        );
        let outcome = self.engine.eval_module_attrs_interruptible(
            &source,
            &resolved,
            move |specifier, referrer, module_type| {
                module_dependency_loader(
                    &page,
                    &cookie_context,
                    &handle,
                    &cache,
                    &fetched,
                    None,
                    &environment,
                    specifier,
                    referrer,
                    module_type,
                )
            },
        );
        match outcome {
            // #dom-worklet-addmodule: a script whose error to rethrow is set
            // rejects the promise with it.
            Err(error) => {
                return Err(format!(
                    "SyntaxError:{resolved}:{}: {}",
                    error.line, error.message
                ));
            }
            // A module that does not parse is a script whose error to
            // rethrow is set.
            Ok(trust_js::ExecutionOutcome::Throw { name, message }) if name == "SyntaxError" => {
                return Err(format!("SyntaxError:{message}"));
            }
            // "Run a module script": an exception is reported, not rejected.
            Ok(trust_js::ExecutionOutcome::Throw { name, message }) => {
                eprintln!("trust: audio worklet {resolved}: uncaught {name}: {message}");
            }
            Ok(_) => {}
        }
        self.modules.insert(url.to_string());
        let _ = self.engine.run_microtasks_interruptible();
        match self.call("workletTake", &[]) {
            Ok(value) => Ok(super::value_string(&mut self.engine, &value)),
            Err(_) => Ok(String::from("[]")),
        }
    }

    fn create(&mut self, node: u32, name: &str, packet: &str) -> Result<(), ProcessorError> {
        let result = self.call(
            "workletCreate",
            &[
                Value::Num(f64::from(node)),
                Value::from_string(name.to_string()),
                Value::from_string(packet.to_string()),
            ],
        );
        let _ = self.engine.run_microtasks_interruptible();
        match result {
            Ok(value) => {
                let text = super::value_string(&mut self.engine, &value);
                if text.is_empty() {
                    Ok(())
                } else {
                    Err(self.error_of(&text))
                }
            }
            Err(error) => Err(self.thrown(error)),
        }
    }

    fn global_port(&mut self, packet: &str) {
        let _ = self.call(
            "workletGlobalPort",
            &[Value::from_string(packet.to_string())],
        );
    }

    fn process(&mut self, call: Call, outputs: &mut [Bus]) -> Result<bool, ProcessorError> {
        let shape = |counts: &mut dyn Iterator<Item = usize>| {
            counts
                .map(|count| count.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        let input_counts: Vec<usize> = call
            .inputs
            .iter()
            .map(|input| input.map_or(0, Bus::channels))
            .collect();
        let inputs = shape(&mut input_counts.iter().copied());
        let outputs_shape = shape(&mut call.output_channels.iter().copied());
        let params = shape(&mut call.params.iter().map(|values| values.len()));
        let node = Value::Num(f64::from(call.node));
        let arrays = match self.call(
            "workletPrepare",
            &[
                node.clone(),
                Value::from_string(inputs),
                Value::from_string(outputs_shape),
                Value::from_string(params),
            ],
        ) {
            Ok(arrays @ Value::Obj(_)) => arrays,
            Ok(_) => return Ok(false),
            Err(error) => return Err(self.thrown(error)),
        };
        // Inputs, then outputs, then parameters, in the order prepared.
        let mut index = 0;
        for bus in call.inputs.iter().flatten() {
            for channel in 0..bus.channels() {
                if let Some(array) = self.element(&arrays, index) {
                    self.engine
                        .ctx()
                        .typed_array_set_bytes(&array, &float_bytes(bus.channel(channel)));
                }
                index += 1;
            }
        }
        let first_output = index;
        index += call.output_channels.iter().sum::<usize>();
        for values in &call.params {
            if let Some(array) = self.element(&arrays, index) {
                self.engine
                    .ctx()
                    .typed_array_set_bytes(&array, &float_bytes(values));
            }
            index += 1;
        }
        let result = match self.call("workletRun", &[node]) {
            Ok(value) => value,
            Err(error) => return Err(self.thrown(error)),
        };
        let active = match &result {
            Value::Bool(active) => *active,
            Value::Obj(_) => {
                let text = self
                    .engine
                    .ctx()
                    .member_get(&result, "error")
                    .ok()
                    .map(|value| super::value_string(&mut self.engine, &value))
                    .unwrap_or_default();
                return Err(self.error_of(&text));
            }
            _ => false,
        };
        let mut index = first_output;
        for (output, &channels) in outputs.iter_mut().zip(&call.output_channels) {
            for channel in 0..channels {
                if let Some(array) = self.element(&arrays, index)
                    && let Some(bytes) = self.engine.ctx().typed_array_bytes(&array)
                {
                    let target = output.channel_mut(channel);
                    for (sample, chunk) in target.iter_mut().zip(bytes.as_chunks::<4>().0) {
                        *sample = f32::from_le_bytes(*chunk);
                    }
                }
                index += 1;
            }
        }
        Ok(active)
    }

    fn release(&mut self, node: u32) {
        let _ = self.call("workletRelease", &[Value::Num(f64::from(node))]);
    }

    fn set_time(&mut self, frame: u64) {
        let _ = self.call("workletTime", &[Value::Num(frame as f64)]);
    }

    fn run_tasks(&mut self) {
        // Port messages (the context's associated task queue), each followed
        // by a microtask checkpoint; bounded so a flood cannot stall audio.
        for _ in 0..64 {
            let ready = matches!(self.call("hasPortTask", &[]), Ok(Value::Bool(true)));
            if !ready {
                break;
            }
            let _ = self.call("runPortTask", &[]);
            let _ = self.engine.run_microtasks_interruptible();
        }
        let _ = self.engine.run_microtasks_interruptible();
    }
}
