//! The page actor's half of the Web Audio API: the prelude's Web Audio
//! block (the control thread) drives each BaseAudioContext's rendering
//! thread (`crate::webaudio::engine`) through `__audio_context_binding`,
//! and the threads' reports come back as host tasks
//! (`LumenHostTask::WebAudio`) that run `trust.webAudioEvent(context, kind,
//! a, b, c)` in the context's Window realm.
//!
//! Script owns the API objects and their validation; this side owns the
//! engines, AudioBuffer contents acquired from script or rendered
//! (#acquire-the-content), PeriodicWaves, and the values script reads
//! synchronously ([[current value]], `reduction`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::{Ctx, HostState, LumenHostTask, Value, host_arg_string};
use crate::webaudio::AudioData;
use crate::webaudio::bus::{ChannelConfig, CountMode, Interpretation};
use crate::webaudio::engine::{Command, Engine, Notice, Realtime};
use crate::webaudio::graph::{GraphMessage, NodeInit};
use crate::webaudio::nodes::{self, Control, Processor};
use crate::webaudio::param::{Automation, Descriptor, Rate};
use crate::webaudio::wave::{Builtin, Wave};

struct ContextEntry {
    engine: Engine,
    /// The Window realm whose script created the context.
    realm: u64,
    next_id: u32,
    /// [[current value]] slots and compressor `reduction`s, by id.
    values: HashMap<u32, Arc<AtomicU32>>,
    /// Each node's entries in `values`.
    node_values: HashMap<u32, Vec<u32>>,
    /// BiquadFilterNode types and IIRFilterNode coefficients, for
    /// getFrequencyResponse().
    filters: HashMap<u32, Filter>,
    /// AnalyserNodes' shared analysis state.
    analysers: HashMap<u32, nodes::analyser::Shared>,
    /// ScriptProcessorNodes' answers to their events.
    scripts: HashMap<u32, nodes::script::Shared>,
    offline: bool,
}

impl Drop for ContextEntry {
    fn drop(&mut self) {
        // An offline rendering thread may be waiting for script.
        for script in self.scripts.values() {
            script.close();
        }
    }
}

/// What getFrequencyResponse() evaluates.
enum Filter {
    Biquad(nodes::biquad::Kind),
    Iir(nodes::iir::Coefficients),
}

#[derive(Default)]
pub(super) struct WebAudioHost {
    contexts: HashMap<u64, ContextEntry>,
    contents: HashMap<u64, Arc<AudioData>>,
    waves: HashMap<u64, Arc<Wave>>,
    next_id: u64,
}

impl WebAudioHost {
    pub(super) fn retained_bytes(&self) -> usize {
        self.contents
            .values()
            .map(|data| data.bytes())
            .sum::<usize>()
            + self.contexts.capacity() * std::mem::size_of::<(u64, ContextEntry)>()
    }

    /// A Window's Document went away: its contexts stop without notifying
    /// it (#AudioContext-methods close(), "in reaction to the document being
    /// unloaded").
    pub(super) fn release_realm(&mut self, realm: u64) {
        self.contexts.retain(|_, entry| entry.realm != realm);
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

/// A notice batch for context `context`, delivered as a host task.
pub(super) struct WebAudioTask {
    pub(super) context: u64,
    pub(super) notices: Vec<Notice>,
}

fn number(args: &[Value], index: usize) -> f64 {
    args.get(index)
        .and_then(Value::as_num_opt)
        .unwrap_or(f64::NAN)
}

fn whole(args: &[Value], index: usize) -> u64 {
    let value = number(args, index);
    if value.is_finite() && value >= 0.0 {
        value as u64
    } else {
        0
    }
}

fn floats(ctx: &mut Ctx, value: Option<&Value>) -> Option<Vec<f32>> {
    let bytes = ctx.typed_array_bytes(value?)?;
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect(),
    )
}

fn doubles(ctx: &mut Ctx, value: Option<&Value>) -> Option<Vec<f64>> {
    let bytes = ctx.typed_array_bytes(value?)?;
    Some(
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|chunk| f64::from_le_bytes(*chunk))
            .collect(),
    )
}

fn float_bytes(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}

fn channel_config(args: &[Value], index: usize) -> ChannelConfig {
    ChannelConfig {
        count: (whole(args, index) as usize).clamp(1, crate::webaudio::MAX_CHANNELS),
        mode: match whole(args, index + 1) {
            0 => CountMode::Max,
            1 => CountMode::ClampedMax,
            _ => CountMode::Explicit,
        },
        interpretation: if whole(args, index + 2) == 1 {
            Interpretation::Discrete
        } else {
            Interpretation::Speakers
        },
    }
}

fn builtin(index: u64) -> Builtin {
    match index {
        1 => Builtin::Square,
        2 => Builtin::Sawtooth,
        3 => Builtin::Triangle,
        _ => Builtin::Sine,
    }
}

/// The output device's preferred rate and channel count, asked once.
fn device(silent: bool) -> (u32, u16) {
    if silent {
        return (48_000, 2);
    }
    static DEVICE: OnceLock<(u32, u16)> = OnceLock::new();
    *DEVICE.get_or_init(crate::media::audio::default_device)
}

fn silent(ctx: &mut Ctx) -> bool {
    cfg!(test)
        || ctx
            .host_mut::<HostState>()
            .is_some_and(|state| state.media.silent)
}

/// `__audio_context_binding(op, …)` for the Web Audio operations.
pub(super) fn binding(ctx: &mut Ctx, op: &str, args: &[Value]) -> Result<Value, Value> {
    match op {
        "device" => {
            let (rate, channels) = device(silent(ctx));
            return Ok(ctx.make_array(vec![
                Value::Num(f64::from(rate)),
                Value::Num(f64::from(channels)),
            ]));
        }
        "realtime" | "offline" => return Ok(create(ctx, op == "realtime", args)),
        "buffer" => return Ok(buffer(ctx, args)),
        "bufferRead" => {
            let (id, channel) = (whole(args, 1), whole(args, 2) as usize);
            let data = ctx
                .host_mut::<HostState>()
                .and_then(|state| state.webaudio.contents.get(&id).cloned());
            let Some(samples) = data.as_ref().and_then(|data| data.channels.get(channel)) else {
                return Ok(Value::Null);
            };
            return ctx.make_array_buffer(&float_bytes(samples));
        }
        "bufferCopy" => {
            // copyFromChannel() from acquired content: `destination`'s
            // leading elements from `offset` on.
            let (id, channel, offset) = (
                whole(args, 1),
                whole(args, 2) as usize,
                whole(args, 3) as usize,
            );
            let data = ctx
                .host_mut::<HostState>()
                .and_then(|state| state.webaudio.contents.get(&id).cloned());
            let Some(samples) = data.as_ref().and_then(|data| data.channels.get(channel)) else {
                return Ok(Value::Undefined);
            };
            let Some(target) = args.get(5) else {
                return Ok(Value::Undefined);
            };
            let room = whole(args, 4) as usize;
            let start = offset.min(samples.len());
            let count = (samples.len() - start).min(room);
            ctx.typed_array_set_bytes(target, &float_bytes(&samples[start..start + count]));
            return Ok(Value::Undefined);
        }
        "bufferRelease" => {
            let id = whole(args, 1);
            if let Some(state) = ctx.host_mut::<HostState>() {
                state.webaudio.contents.remove(&id);
            }
            return Ok(Value::Undefined);
        }
        "wave" => {
            let real = floats(ctx, args.get(1)).unwrap_or_default();
            let imag = floats(ctx, args.get(2)).unwrap_or_default();
            let normalize = args
                .get(3)
                .is_some_and(|value| matches!(value, Value::Bool(true)));
            let wave = Arc::new(Wave::new(&real, &imag, normalize));
            let Some(state) = ctx.host_mut::<HostState>() else {
                return Ok(Value::Num(0.0));
            };
            let id = state.webaudio.next();
            state.webaudio.waves.insert(id, wave);
            return Ok(Value::Num(id as f64));
        }
        "waveRelease" => {
            let id = whole(args, 1);
            if let Some(state) = ctx.host_mut::<HostState>() {
                state.webaudio.waves.remove(&id);
            }
            return Ok(Value::Undefined);
        }
        _ => {}
    }
    let context = whole(args, 1);
    // Operations of one context. Values script must convert first (curves)
    // are read before the context is borrowed.
    let curve = if op == "automate" && whole(args, 3) == 5 {
        floats(ctx, args.get(4))
    } else {
        None
    };
    // The node kind or control name.
    let name = match op {
        "node" => host_arg_string(ctx, args, 2),
        "control" => host_arg_string(ctx, args, 3),
        _ => String::new(),
    };
    let descriptors = if op == "node" {
        doubles(ctx, args.get(8)).unwrap_or_default()
    } else {
        Vec::new()
    };
    // IIRFilterNode coefficients and a WaveShaperNode curve.
    let (feedforward, feedback) = if op == "node" && name == "iir" {
        (
            doubles(ctx, args.get(9)).unwrap_or_default(),
            doubles(ctx, args.get(10)).unwrap_or_default(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let shaper_curve = match (op, name.as_str()) {
        ("node", "shaper") => floats(ctx, args.get(9)),
        ("control", "curve") => floats(ctx, args.get(4)),
        _ => None,
    };
    match op {
        "decode" => return Ok(decode(ctx, context, args)),
        "frequencyResponse" => return Ok(frequency_response(ctx, context, args)),
        "analyser" => return Ok(analyse(ctx, context, args)),
        _ => {}
    }
    let Some(state) = ctx.host_mut::<HostState>() else {
        return Ok(Value::Undefined);
    };
    // createMediaElementSource(): the element's audio leaves its output
    // for the graph.
    let element_tap =
        (op == "node" && name == "media-element").then(|| state.media.tap(whole(args, 9) as usize));
    let host = &mut state.webaudio;
    if op == "drop" {
        host.contexts.remove(&context);
        return Ok(Value::Undefined);
    }
    // Resolve the references to contents and waves a control names.
    let content = |host: &WebAudioHost, id: u64| host.contents.get(&id).cloned();
    let wave = |host: &WebAudioHost, id: f64| {
        if id < 0.0 {
            Some(Wave::builtin(builtin((-id) as u64 - 1)))
        } else {
            host.waves.get(&(id as u64)).cloned()
        }
    };
    let control = match op {
        "control" => {
            let (a, b, c) = (number(args, 4), number(args, 5), number(args, 6));
            match name.as_str() {
                "start" => Some(Control::Start {
                    when: a,
                    offset: if b.is_finite() { b } else { 0.0 },
                    duration: c.is_finite().then_some(c),
                }),
                "stop" => Some(Control::Stop(a)),
                "wave" => wave(host, a).map(Control::Wave),
                "buffer" => Some(Control::Buffer(if a > 0.0 {
                    content(host, a as u64)
                } else {
                    None
                })),
                "loop" => Some(Control::Loop(a != 0.0)),
                "loopStart" => Some(Control::LoopStart(a)),
                "loopEnd" => Some(Control::LoopEnd(a)),
                "type" => Some(Control::FilterType(a as u64)),
                "curve" => Some(Control::Curve(shaper_curve.clone().map(Arc::from))),
                "oversample" => Some(Control::Oversample(a as usize)),
                "response" => {
                    let quantum = host
                        .contexts
                        .get(&context)
                        .map_or(128, |entry| entry.engine.quantum);
                    Some(Control::Response(if a > 0.0 {
                        content(host, a as u64)
                            .and_then(|data| {
                                nodes::convolver::Response::new(&data, b != 0.0, quantum)
                            })
                            .map(Arc::new)
                    } else {
                        None
                    }))
                }
                _ => None,
            }
        }
        _ => None,
    };
    let initial_wave = if op == "node" && name == "oscillator" {
        wave(host, number(args, 9))
    } else {
        None
    };
    // A ScriptProcessorNode's filled outputBuffer, acquired by script.
    let answer = if op == "scriptOutput" {
        content(host, whole(args, 3))
    } else {
        None
    };
    let Some(entry) = host.contexts.get_mut(&context) else {
        return Ok(Value::Undefined);
    };
    let send =
        |entry: &ContextEntry, message: GraphMessage| entry.engine.send(Command::Graph(message));
    match op {
        "start" => entry.engine.send(Command::Start(whole(args, 2) as u32)),
        "suspend" => entry.engine.send(Command::Suspend(whole(args, 2) as u32)),
        "close" => {
            for script in entry.scripts.values() {
                script.close();
            }
            entry.engine.send(Command::Close(whole(args, 2) as u32))
        }
        "scriptOutput" => {
            // A ScriptProcessorNode's filled outputBuffer.
            if let (Some(script), Some(data)) =
                (entry.scripts.get(&(whole(args, 2) as u32)), answer)
            {
                script.answer(data);
            }
        }
        "render" => entry.engine.send(Command::Render(whole(args, 2) as usize)),
        "suspendAt" => entry.engine.send(Command::SuspendAt(whole(args, 2))),
        "frame" => return Ok(Value::Num(entry.engine.frame() as f64)),
        "latency" => {
            let (base, output) = (entry.engine.base_latency(), entry.engine.output_latency());
            return Ok(ctx.make_array(vec![Value::Num(base), Value::Num(output)]));
        }
        "outputTime" => return Ok(Value::Num(entry.engine.output_time())),
        "value" => {
            let value = entry
                .values
                .get(&(whole(args, 2) as u32))
                .map_or(f64::NAN, |value| {
                    f64::from(f32::from_bits(value.load(Ordering::Relaxed)))
                });
            return Ok(Value::Num(value));
        }
        "node" => {
            let kind = name.as_str();
            let (inputs, outputs) = (whole(args, 3) as usize, whole(args, 4) as usize);
            let channel = channel_config(args, 5);
            let id = entry.next_id;
            let mut params = Vec::new();
            let mut values = Vec::new();
            for (index, descriptor) in descriptors.as_chunks::<5>().0.iter().enumerate() {
                let param_id = id + 1 + index as u32;
                let current = Arc::new(AtomicU32::new((descriptor[0] as f32).to_bits()));
                entry.values.insert(param_id, current.clone());
                values.push(param_id);
                params.push((
                    param_id,
                    Descriptor {
                        default: descriptor[0] as f32,
                        min: descriptor[1] as f32,
                        max: descriptor[2] as f32,
                        rate: if descriptor[3] == 1.0 {
                            Rate::K
                        } else {
                            Rate::A
                        },
                        compound: descriptor[4] != 0.0,
                    },
                    current,
                ));
            }
            entry.next_id += 1 + params.len() as u32;
            let processor: Box<dyn Processor> = match kind {
                "destination" => Box::new(nodes::Destination),
                "gain" => Box::new(nodes::Gain),
                "oscillator" => Box::new(nodes::Oscillator::new(
                    initial_wave.unwrap_or_else(|| Wave::builtin(Builtin::Sine)),
                )),
                "buffer-source" => Box::new(nodes::BufferSource::new(
                    number(args, 9) != 0.0,
                    number(args, 10),
                    number(args, 11),
                )),
                "compressor" => {
                    let reduction = Arc::new(AtomicU32::new(0));
                    entry.values.insert(id, reduction.clone());
                    values.push(id);
                    Box::new(nodes::Compressor::new(reduction))
                }
                "constant" => Box::new(nodes::ConstantSource::new()),
                "stereo-panner" => Box::new(nodes::StereoPanner),
                "splitter" => Box::new(nodes::Splitter),
                "merger" => Box::new(nodes::Merger),
                "delay" => Box::new(nodes::Delay::new(
                    number(args, 9),
                    entry.engine.rate,
                    entry.engine.quantum,
                )),
                "biquad" => {
                    let kind = nodes::biquad::Kind::from_index(whole(args, 9));
                    entry.filters.insert(id, Filter::Biquad(kind));
                    Box::new(nodes::Biquad::new(kind))
                }
                "iir"
                    if !feedforward.is_empty() && feedback.first().is_some_and(|&a0| a0 != 0.0) =>
                {
                    let coefficients = nodes::iir::Coefficients::new(&feedforward, &feedback);
                    entry.filters.insert(id, Filter::Iir(coefficients.clone()));
                    Box::new(nodes::Iir::new(coefficients))
                }
                "shaper" => Box::new(nodes::WaveShaper::new(
                    shaper_curve.map(Arc::from),
                    whole(args, 10) as usize,
                )),
                "analyser" => {
                    let shared = nodes::analyser::shared();
                    entry.analysers.insert(id, shared.clone());
                    Box::new(nodes::Analyser::new(shared))
                }
                "convolver" => Box::new(nodes::Convolver::new()),
                "processor" => {
                    let shared = nodes::script::Shared::default();
                    entry.scripts.insert(id, shared.clone());
                    Box::new(nodes::ScriptProcessor::new(
                        whole(args, 9) as usize,
                        channel.count,
                        whole(args, 10) as usize,
                        entry.offline,
                        shared,
                    ))
                }
                "media-element" => match element_tap {
                    Some(tap) => Box::new(nodes::MediaElementSource::new(tap)),
                    None => Box::new(nodes::Silent { channels: 1 }),
                },
                _ => Box::new(nodes::Silent {
                    channels: whole(args, 9) as usize,
                }),
            };
            entry.node_values.insert(id, values);
            send(
                entry,
                GraphMessage::Create {
                    id,
                    init: Box::new(NodeInit {
                        processor,
                        inputs,
                        outputs,
                        channel,
                        params,
                    }),
                },
            );
            return Ok(Value::Num(f64::from(id)));
        }
        "release" => {
            let node = whole(args, 2) as u32;
            for value in entry.node_values.remove(&node).unwrap_or_default() {
                entry.values.remove(&value);
            }
            entry.filters.remove(&node);
            entry.analysers.remove(&node);
            if let Some(script) = entry.scripts.remove(&node) {
                script.close();
            }
            send(entry, GraphMessage::Release(node));
        }
        "connect" | "disconnect" => {
            let (from, output, to, input) = (
                whole(args, 2) as u32,
                whole(args, 3) as usize,
                whole(args, 4) as u32,
                whole(args, 5) as usize,
            );
            send(
                entry,
                if op == "connect" {
                    GraphMessage::Connect {
                        from,
                        output,
                        to,
                        input,
                    }
                } else {
                    GraphMessage::Disconnect {
                        from,
                        output,
                        to,
                        input,
                    }
                },
            );
        }
        "connectParam" | "disconnectParam" => {
            let (from, output, param) = (
                whole(args, 2) as u32,
                whole(args, 3) as usize,
                whole(args, 4) as u32,
            );
            send(
                entry,
                if op == "connectParam" {
                    GraphMessage::ConnectParam {
                        from,
                        output,
                        param,
                    }
                } else {
                    GraphMessage::DisconnectParam {
                        from,
                        output,
                        param,
                    }
                },
            );
        }
        "channel" => {
            let node = whole(args, 2) as u32;
            send(
                entry,
                GraphMessage::Channel {
                    node,
                    config: channel_config(args, 3),
                },
            );
        }
        "automate" => {
            // (context, param, op, a, b, c, now)
            let param = whole(args, 2) as u32;
            let (a, b, c, now) = (
                number(args, 4),
                number(args, 5),
                number(args, 6),
                number(args, 7),
            );
            let automation = match whole(args, 3) {
                0 => {
                    if let Some(value) = entry.values.get(&param) {
                        value.store((a as f32).to_bits(), Ordering::Relaxed);
                    }
                    Automation::Value(a as f32)
                }
                1 => Automation::SetValue {
                    value: a as f32,
                    time: b,
                },
                2 => Automation::LinearRamp {
                    value: a as f32,
                    time: b,
                },
                3 => Automation::ExponentialRamp {
                    value: a as f32,
                    time: b,
                },
                4 => Automation::SetTarget {
                    target: a as f32,
                    time: b,
                    time_constant: c as f32,
                },
                5 => Automation::SetCurve {
                    values: curve.unwrap_or_default().into(),
                    time: b,
                    duration: c,
                },
                6 => Automation::Cancel(a),
                7 => Automation::CancelAndHold(a),
                _ => Automation::Rate(if a == 1.0 { Rate::K } else { Rate::A }),
            };
            send(
                entry,
                GraphMessage::Param {
                    param,
                    automation,
                    now,
                },
            );
        }
        "control" => {
            if let (Some(Control::FilterType(kind)), Some(Filter::Biquad(current))) =
                (&control, entry.filters.get_mut(&(whole(args, 2) as u32)))
            {
                *current = nodes::biquad::Kind::from_index(*kind);
            }
            if let Some(control) = control {
                send(
                    entry,
                    GraphMessage::Control {
                        node: whole(args, 2) as u32,
                        control,
                    },
                );
            }
        }
        _ => {}
    }
    Ok(Value::Undefined)
}

fn create(ctx: &mut Ctx, realtime: bool, args: &[Value]) -> Value {
    let silent = silent(ctx);
    let realm = ctx.host_job_context();
    let Some(state) = ctx.host_mut::<HostState>() else {
        return Value::Num(0.0);
    };
    let id = state.webaudio.next();
    // A realm without an event loop (some tests) renders but hears nothing.
    let sender = Mutex::new(state.task_events.clone());
    let notify = Box::new(move |notices: Vec<Notice>| {
        if let Ok(sender) = sender.lock()
            && let Some(sender) = sender.as_ref()
        {
            let _ = sender.send(LumenHostTask::WebAudio(WebAudioTask {
                context: id,
                notices,
            }));
        }
    });
    let rate = number(args, 1) as f32;
    let quantum = (whole(args, 2) as usize).max(1);
    let channels = (whole(args, 3) as usize).clamp(1, crate::webaudio::MAX_CHANNELS);
    let engine = if realtime {
        let latency = number(args, 4);
        Engine::realtime(
            Realtime {
                rate,
                quantum,
                channels: channels as u16,
                latency: if latency.is_finite() { latency } else { 0.02 },
                silent: silent
                    || args
                        .get(5)
                        .is_some_and(|value| matches!(value, Value::Bool(true))),
            },
            notify,
        )
    } else {
        Engine::offline(rate, quantum, channels, notify)
    };
    state.webaudio.contexts.insert(
        id,
        ContextEntry {
            engine,
            realm,
            next_id: 1,
            values: HashMap::new(),
            node_values: HashMap::new(),
            filters: HashMap::new(),
            analysers: HashMap::new(),
            scripts: HashMap::new(),
            offline: !realtime,
        },
    );
    Value::Num(id as f64)
}

/// `frequencyResponse(context, node, frequencyHz, magResponse,
/// phaseResponse)`: BiquadFilterNode's (from the parameters' [[current
/// value]]s) or IIRFilterNode's getFrequencyResponse(). Frequencies outside
/// [0, sampleRate/2] give NaN.
fn frequency_response(ctx: &mut Ctx, context: u64, args: &[Value]) -> Value {
    let node = whole(args, 2) as u32;
    let frequencies = floats(ctx, args.get(3)).unwrap_or_default();
    let Some(state) = ctx.host_mut::<HostState>() else {
        return Value::Undefined;
    };
    let Some(entry) = state.webaudio.contexts.get(&context) else {
        return Value::Undefined;
    };
    let rate = entry.engine.rate;
    let current = |id: u32| {
        entry
            .values
            .get(&id)
            .map_or(0.0, |value| f32::from_bits(value.load(Ordering::Relaxed)))
    };
    let evaluate: Box<dyn Fn(f64) -> (f64, f64)> = match entry.filters.get(&node) {
        Some(Filter::Biquad(kind)) => {
            let frequency =
                nodes::biquad::computed_frequency(current(node + 1), current(node + 2), rate);
            let coefficients = nodes::biquad::Coefficients::compute(
                *kind,
                frequency,
                f64::from(current(node + 3)),
                f64::from(current(node + 4)),
                f64::from(rate),
            );
            Box::new(move |hz| coefficients.response(hz, f64::from(rate)))
        }
        Some(Filter::Iir(coefficients)) => {
            let coefficients = coefficients.clone();
            Box::new(move |hz| coefficients.response(hz, f64::from(rate)))
        }
        None => return Value::Undefined,
    };
    let nyquist = f64::from(rate) / 2.0;
    let (mut magnitudes, mut phases) = (Vec::new(), Vec::new());
    for &frequency in &frequencies {
        let frequency = f64::from(frequency);
        let (magnitude, phase) = if (0.0..=nyquist).contains(&frequency) {
            evaluate(frequency)
        } else {
            (f64::NAN, f64::NAN)
        };
        magnitudes.push(magnitude as f32);
        phases.push(phase as f32);
    }
    if let Some(target) = args.get(4) {
        ctx.typed_array_set_bytes(target, &float_bytes(&magnitudes));
    }
    if let Some(target) = args.get(5) {
        ctx.typed_array_set_bytes(target, &float_bytes(&phases));
    }
    Value::Undefined
}

/// `analyser(context, node, op, fftSize, smoothing, minDecibels,
/// maxDecibels, array, byteLength)`: an AnalyserNode's data written into
/// `array` (#AnalyserNode-methods): "floatFrequency", "byteFrequency",
/// "floatTime", "byteTime", or "reset" (fftSize changed).
fn analyse(ctx: &mut Ctx, context: u64, args: &[Value]) -> Value {
    let node = whole(args, 2) as u32;
    let op = host_arg_string(ctx, args, 3);
    let size = (whole(args, 4) as usize).clamp(32, nodes::analyser::MAX_FFT);
    let (a, b, c) = (number(args, 5), number(args, 6), number(args, 7));
    let Some(shared) = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.webaudio.contexts.get(&context))
        .and_then(|entry| entry.analysers.get(&node).cloned())
    else {
        return Value::Undefined;
    };
    let bytes: Vec<u8> = {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        match op.as_str() {
            "reset" => {
                state.reset();
                return Value::Undefined;
            }
            "floatFrequency" => float_bytes(state.frequency(size, a)),
            "byteFrequency" => {
                // b[k] = floor(255 / (dBmax - dBmin) · (Y[k] - dBmin)), clipped.
                let (min, max) = (b, c);
                state
                    .frequency(size, a)
                    .iter()
                    .map(|&db| {
                        let value = (255.0 / (max - min) * (f64::from(db) - min)).floor();
                        if value.is_nan() {
                            0
                        } else {
                            value.clamp(0.0, 255.0) as u8
                        }
                    })
                    .collect()
            }
            "floatTime" => float_bytes(&state.time_domain(size)),
            "byteTime" => state
                .time_domain(size)
                .iter()
                .map(|&x| (128.0 * (1.0 + f64::from(x))).floor().clamp(0.0, 255.0) as u8)
                .collect(),
            _ => return Value::Undefined,
        }
    };
    if let Some(target) = args.get(8) {
        // The view's own length (in bytes, from script) bounds the write:
        // excess data is dropped.
        let room = whole(args, 9) as usize;
        ctx.typed_array_set_bytes(target, &bytes[..bytes.len().min(room)]);
    }
    Value::Undefined
}

/// `decode(context, bytes)`: decodeAudioData()'s decoding operation on a
/// decoding thread; its result arrives as a `decoded` task. Returns the job
/// id (0 when the context is gone).
fn decode(ctx: &mut Ctx, context: u64, args: &[Value]) -> Value {
    let bytes = args
        .get(2)
        .and_then(|value| ctx.buffer_source_bytes(value, false))
        .unwrap_or_default();
    let Some(state) = ctx.host_mut::<HostState>() else {
        return Value::Num(0.0);
    };
    let Some(rate) = state
        .webaudio
        .contexts
        .get(&context)
        .map(|entry| entry.engine.rate)
    else {
        return Value::Num(0.0);
    };
    let job = state.webaudio.next();
    let sender = state.task_events.clone();
    crate::webaudio::decode::spawn(bytes, rate, move |data| {
        if let Some(sender) = sender {
            let _ = sender.send(LumenHostTask::WebAudio(WebAudioTask {
                context,
                notices: vec![Notice::Decoded {
                    job,
                    data: data.map(Arc::new),
                }],
            }));
        }
    });
    Value::Num(job as f64)
}

/// `buffer(rate, length, ...channels)`: acquire script's channel data as a
/// content (#acquire-the-content step 3). A detached channel makes the
/// whole content zero-length.
fn buffer(ctx: &mut Ctx, args: &[Value]) -> Value {
    let rate = number(args, 1) as f32;
    let length = whole(args, 2) as usize;
    let mut channels = Vec::new();
    for value in args.iter().skip(3) {
        match floats(ctx, Some(value)) {
            Some(samples) if samples.len() == length => channels.push(samples.into_boxed_slice()),
            _ => {
                channels.clear();
                break;
            }
        }
    }
    let data = if channels.is_empty() {
        AudioData {
            rate,
            length: 0,
            channels: Vec::new(),
        }
    } else {
        AudioData {
            rate,
            length,
            channels,
        }
    };
    let Some(state) = ctx.host_mut::<HostState>() else {
        return Value::Num(0.0);
    };
    let id = state.webaudio.next();
    state.webaudio.contents.insert(id, Arc::new(data));
    Value::Num(id as f64)
}

/// Keep `data` as a content for an AudioBuffer: (id, length, channels).
fn store(engine: &mut lumen::Engine, data: Arc<AudioData>) -> Option<(f64, f64, f64)> {
    let (length, channels) = (data.length as f64, data.channels.len() as f64);
    let state = engine.ctx().host_mut::<HostState>()?;
    let id = state.webaudio.next();
    state.webaudio.contents.insert(id, data);
    Some((id as f64, length, channels))
}

/// Deliver a context's notices to its realm.
pub(super) fn dispatch(engine: &mut lumen::Engine, task: WebAudioTask) -> Result<(), String> {
    let Some(realm) = engine
        .ctx()
        .host_mut::<HostState>()
        .and_then(|state| state.webaudio.contexts.get(&task.context))
        .map(|entry| entry.realm)
    else {
        return Ok(());
    };
    let context = Value::Num(task.context as f64);
    let mut calls = Vec::with_capacity(task.notices.len());
    for notice in task.notices {
        let (kind, a, b, c, d) = match notice {
            Notice::Ended(node) => ("ended", f64::from(node), 0.0, 0.0, 0.0),
            Notice::Running(token) => ("running", f64::from(token), 0.0, 0.0, 0.0),
            Notice::Suspended(token) => ("suspended", f64::from(token), 0.0, 0.0, 0.0),
            Notice::Closed(token) => ("closed", f64::from(token), 0.0, 0.0, 0.0),
            Notice::OfflineSuspended(frame) => ("offlineSuspended", frame as f64, 0.0, 0.0, 0.0),
            Notice::SuspendMissed(frame) => ("suspendMissed", frame as f64, 0.0, 0.0, 0.0),
            Notice::RenderFailed => ("renderFailed", 0.0, 0.0, 0.0, 0.0),
            Notice::Rendered(data) => {
                let Some((id, length, channels)) = store(engine, data) else {
                    return Ok(());
                };
                ("rendered", id, length, channels, 0.0)
            }
            Notice::ScriptProcess {
                node,
                playback_time,
                input,
            } => {
                let Some((id, _, _)) = store(engine, input) else {
                    return Ok(());
                };
                ("audioprocess", f64::from(node), id, playback_time, 0.0)
            }
            Notice::Decoded { job, data } => match data.and_then(|data| store(engine, data)) {
                Some((id, length, channels)) => ("decoded", job as f64, id, length, channels),
                None => ("decoded", job as f64, 0.0, 0.0, 0.0),
            },
        };
        calls.push([
            context.clone(),
            Value::from_string(kind.to_string()),
            Value::Num(a),
            Value::Num(b),
            Value::Num(c),
            Value::Num(d),
        ]);
    }
    super::run_in_context_realm(engine, realm, "Web Audio task", move |engine| {
        for args in calls {
            if let Err(error) =
                super::host_call_trust_interruptible(engine.ctx(), "webAudioEvent", &args)
            {
                return super::host_task_failure(engine.ctx(), error, "Web Audio task");
            }
        }
        Ok(())
    })
}
