//! The audio graph on the rendering thread (#rendering-loop "process a
//! render quantum").
//!
//! Nodes and parameters are addressed by the ids the control thread gave
//! them; internally they live in slabs whose indices edges refer to. The
//! node order is recomputed only when the topology changes: Tarjan's
//! strongly connected components over "depends on" edges (a node's input
//! connections and its parameters' input connections) come out with every
//! component after the components it depends on, which is an order the
//! spec's ordering algorithm accepts. Nodes in a cycle are muted.
//!
//! A node that is not actively processing (#AudioNode-actively-processing)
//! outputs a single channel of silence and is not processed. A node the
//! control thread released (its object was collected) stays while it is
//! actively processing, so a tail still sounds; then it is removed.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use super::bus::{Bus, ChannelConfig, Interpretation};
use super::engine::Notice;
use super::nodes::{Control, Params, Processor, Scope};
use super::param::{Automation, Descriptor, Param};

/// A new node, built on the control thread.
pub struct NodeInit {
    pub processor: Box<dyn Processor>,
    pub inputs: usize,
    pub outputs: usize,
    pub channel: ChannelConfig,
    /// Its AudioParams: id, descriptor, and the shared [[current value]].
    pub params: Vec<(u32, Descriptor, Arc<AtomicU32>)>,
}

/// A change to the graph, from the control thread.
pub enum GraphMessage {
    Create {
        id: u32,
        init: Box<NodeInit>,
    },
    /// The node's object was collected; it goes once it is silent.
    Release(u32),
    Connect {
        from: u32,
        output: usize,
        to: u32,
        input: usize,
    },
    Disconnect {
        from: u32,
        output: usize,
        to: u32,
        input: usize,
    },
    ConnectParam {
        from: u32,
        output: usize,
        param: u32,
    },
    DisconnectParam {
        from: u32,
        output: usize,
        param: u32,
    },
    Channel {
        node: u32,
        config: ChannelConfig,
    },
    Param {
        param: u32,
        automation: Automation,
        now: f64,
    },
    Control {
        node: u32,
        control: Control,
    },
}

struct Node {
    id: u32,
    processor: Box<dyn Processor>,
    /// A DelayNode, which can break a cycle.
    delay: bool,
    channel: ChannelConfig,
    /// Per input: the (node index, output) connections into it.
    inputs: Vec<Vec<(usize, usize)>>,
    input_buses: Vec<Bus>,
    outputs: Vec<Bus>,
    params: Vec<usize>,
    released: bool,
    /// Actively processing during the last rendered quantum.
    active: bool,
    /// The processor reported output that may continue with silent input.
    tail: bool,
    /// Part of a cycle without a delay: muted.
    muted: bool,
    /// Something is connected to an input or an output.
    connected: bool,
}

/// One entry of the processing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Node(usize),
    /// The DelayReader of a DelayNode in a cycle (a source).
    Read(usize),
    /// Its DelayWriter, after every other node.
    Write(usize),
}

pub(super) struct ParamSlot {
    pub(super) param: Param,
    id: u32,
    inputs: Vec<(usize, usize)>,
}

pub struct Graph {
    nodes: Vec<Option<Node>>,
    free_nodes: Vec<usize>,
    node_ids: HashMap<u32, usize>,
    params: Vec<Option<ParamSlot>>,
    free_params: Vec<usize>,
    param_ids: HashMap<u32, usize>,
    order: Vec<Step>,
    dirty: bool,
    destination: Option<usize>,
    mono: Bus,
    pub rate: f32,
    pub quantum: usize,
    /// [[current frame]].
    pub frame: u64,
}

impl Graph {
    pub fn new(rate: f32, quantum: usize) -> Self {
        Self {
            nodes: Vec::new(),
            free_nodes: Vec::new(),
            node_ids: HashMap::new(),
            params: Vec::new(),
            free_params: Vec::new(),
            param_ids: HashMap::new(),
            order: Vec::new(),
            dirty: false,
            destination: None,
            mono: Bus::default(),
            rate,
            quantum,
            frame: 0,
        }
    }

    /// The destination's input for the last rendered quantum (one silent
    /// channel when nothing active reaches it).
    pub fn destination_output(&self) -> Option<&Bus> {
        let node = self.nodes[self.destination?].as_ref()?;
        Some(&node.outputs[0])
    }

    /// The destination's channelInterpretation (for mixing to the device).
    pub fn destination_interpretation(&self) -> Interpretation {
        self.destination
            .and_then(|index| self.nodes[index].as_ref())
            .map_or(Interpretation::Speakers, |node| node.channel.interpretation)
    }

    pub fn node_count(&self) -> usize {
        self.node_ids.len()
    }

    /// Apply a control message (#rendering-loop step 2).
    pub fn apply(&mut self, message: GraphMessage) {
        match message {
            GraphMessage::Create { id, init } => self.create(id, *init),
            GraphMessage::Release(id) => {
                if let Some(node) = self.node_mut(id) {
                    node.released = true;
                }
            }
            GraphMessage::Connect {
                from,
                output,
                to,
                input,
            } => {
                let (Some(&source), Some(&target)) =
                    (self.node_ids.get(&from), self.node_ids.get(&to))
                else {
                    return;
                };
                if let Some(node) = self.nodes[target].as_mut()
                    && let Some(edges) = node.inputs.get_mut(input)
                    && !edges.contains(&(source, output))
                {
                    edges.push((source, output));
                    self.dirty = true;
                }
            }
            GraphMessage::Disconnect {
                from,
                output,
                to,
                input,
            } => {
                let (Some(&source), Some(&target)) =
                    (self.node_ids.get(&from), self.node_ids.get(&to))
                else {
                    return;
                };
                if let Some(node) = self.nodes[target].as_mut()
                    && let Some(edges) = node.inputs.get_mut(input)
                {
                    edges.retain(|&edge| edge != (source, output));
                    self.dirty = true;
                }
            }
            GraphMessage::ConnectParam {
                from,
                output,
                param,
            } => {
                let (Some(&source), Some(&target)) =
                    (self.node_ids.get(&from), self.param_ids.get(&param))
                else {
                    return;
                };
                if let Some(slot) = self.params[target].as_mut()
                    && !slot.inputs.contains(&(source, output))
                {
                    slot.inputs.push((source, output));
                    self.dirty = true;
                }
            }
            GraphMessage::DisconnectParam {
                from,
                output,
                param,
            } => {
                let (Some(&source), Some(&target)) =
                    (self.node_ids.get(&from), self.param_ids.get(&param))
                else {
                    return;
                };
                if let Some(slot) = self.params[target].as_mut() {
                    slot.inputs.retain(|&edge| edge != (source, output));
                    self.dirty = true;
                }
            }
            GraphMessage::Channel { node, config } => {
                if let Some(node) = self.node_mut(node) {
                    node.channel = config;
                }
            }
            GraphMessage::Param {
                param,
                automation,
                now,
            } => {
                if let Some(&index) = self.param_ids.get(&param)
                    && let Some(slot) = self.params[index].as_mut()
                {
                    slot.param.apply(automation, now);
                }
            }
            GraphMessage::Control { node, control } => {
                if let Some(node) = self.node_mut(node) {
                    node.processor.control(control);
                }
            }
        }
    }

    fn node_mut(&mut self, id: u32) -> Option<&mut Node> {
        let index = *self.node_ids.get(&id)?;
        self.nodes[index].as_mut()
    }

    fn create(&mut self, id: u32, init: NodeInit) {
        if self.node_ids.contains_key(&id) {
            return;
        }
        let index = self.free_nodes.pop().unwrap_or_else(|| {
            self.nodes.push(None);
            self.nodes.len() - 1
        });
        let mut params = Vec::with_capacity(init.params.len());
        for (param_id, descriptor, current) in init.params {
            let slot = ParamSlot {
                param: Param::new(descriptor, current),
                id: param_id,
                inputs: Vec::new(),
            };
            let param_index = match self.free_params.pop() {
                Some(free) => {
                    self.params[free] = Some(slot);
                    free
                }
                None => {
                    self.params.push(Some(slot));
                    self.params.len() - 1
                }
            };
            self.param_ids.insert(param_id, param_index);
            params.push(param_index);
        }
        let delay = init.processor.is_delay();
        if self.destination.is_none() {
            // The first node of a context is its destination.
            self.destination = Some(index);
        }
        self.nodes[index] = Some(Node {
            id,
            processor: init.processor,
            delay,
            channel: init.channel,
            inputs: vec![Vec::new(); init.inputs],
            input_buses: vec![Bus::default(); init.inputs],
            outputs: vec![Bus::silent(self.quantum); init.outputs],
            params,
            released: false,
            active: false,
            tail: false,
            muted: false,
            connected: false,
        });
        self.node_ids.insert(id, index);
        self.dirty = true;
    }

    fn remove(&mut self, index: usize) {
        let Some(node) = self.nodes[index].take() else {
            return;
        };
        self.node_ids.remove(&node.id);
        for param in node.params {
            if let Some(slot) = self.params[param].take() {
                self.param_ids.remove(&slot.id);
            }
            self.free_params.push(param);
        }
        for other in self.nodes.iter_mut().flatten() {
            for edges in &mut other.inputs {
                edges.retain(|&(source, _)| source != index);
            }
        }
        for slot in self.params.iter_mut().flatten() {
            slot.inputs.retain(|&(source, _)| source != index);
        }
        self.free_nodes.push(index);
        self.dirty = true;
    }

    /// Order the nodes (#rendering-loop step 4.2): dependencies first.
    /// A DelayNode in a cycle becomes a DelayReader (a source, in order)
    /// and a DelayWriter (after everything else); nodes still in a cycle
    /// are muted.
    fn sort(&mut self) {
        self.dirty = false;
        let count = self.nodes.len();
        let mut depends: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (index, node) in self.nodes.iter().enumerate() {
            let Some(node) = node else { continue };
            let list = &mut depends[index];
            for edges in &node.inputs {
                list.extend(edges.iter().map(|&(source, _)| source));
            }
            for &param in &node.params {
                if let Some(slot) = &self.params[param] {
                    list.extend(slot.inputs.iter().map(|&(source, _)| source));
                }
            }
        }
        let present: Vec<bool> = self.nodes.iter().map(Option::is_some).collect();
        let mut breakers = Vec::new();
        for (component, cycle) in components(&present, &depends) {
            if cycle {
                breakers.extend(
                    component
                        .into_iter()
                        .filter(|&index| self.nodes[index].as_ref().is_some_and(|node| node.delay)),
                );
            }
        }
        for &breaker in &breakers {
            depends[breaker].clear();
        }
        let mut order = Vec::with_capacity(count + breakers.len());
        let mut muted = vec![false; count];
        for (component, cycle) in components(&present, &depends) {
            for index in component {
                muted[index] = cycle;
                order.push(if breakers.contains(&index) {
                    Step::Read(index)
                } else {
                    Step::Node(index)
                });
            }
        }
        order.extend(breakers.iter().map(|&index| Step::Write(index)));
        // Nodes with a connection on either side.
        let mut connected = vec![false; count];
        for (index, list) in depends.iter().enumerate() {
            if !list.is_empty() {
                connected[index] = true;
            }
            for &source in list {
                connected[source] = true;
            }
        }
        for &breaker in &breakers {
            connected[breaker] = true;
        }
        for (index, node) in self.nodes.iter_mut().enumerate() {
            if let Some(node) = node {
                node.muted = muted[index];
                node.connected = connected[index];
            }
        }
        self.order = order;
    }

    fn compute_param(
        params: &mut [Option<ParamSlot>],
        nodes: &[Option<Node>],
        mono: &mut Bus,
        index: usize,
        frame: u64,
        rate: f32,
        frames: usize,
    ) {
        let Some(slot) = params[index].as_mut() else {
            return;
        };
        slot.param.compute_intrinsic(frame, rate, frames);
        let mut mixed = false;
        if !slot.inputs.is_empty() {
            mono.reset(1, frames);
            for &(source, output) in &slot.inputs {
                if let Some(node) = &nodes[source]
                    && node.active
                {
                    // Sum and down-mix to mono (#rendering-loop step 4.5.1).
                    mono.mix_from(&node.outputs[output], Interpretation::Speakers);
                    mixed = true;
                }
            }
        }
        slot.param
            .finish(if mixed { Some(mono.channel(0)) } else { None });
    }

    /// Mix each input of `node` from the outputs of the nodes connected to
    /// it; returns whether any of them is actively processing.
    fn mix_inputs(nodes: &[Option<Node>], node: &mut Node, frames: usize) -> bool {
        let mut any_active = false;
        for (input, edges) in node.inputs.iter().enumerate() {
            let channels = node.channel.computed(edges.iter().map(|&(source, output)| {
                nodes[source]
                    .as_ref()
                    .map_or(1, |node| node.outputs[output].channels())
            }));
            let bus = &mut node.input_buses[input];
            bus.reset(channels, frames);
            for &(source, output) in edges {
                if let Some(other) = &nodes[source]
                    && other.active
                {
                    any_active = true;
                    bus.mix_from(&other.outputs[output], node.channel.interpretation);
                }
            }
        }
        any_active
    }

    /// Render one quantum (#rendering-loop step 4) and advance
    /// [[current frame]].
    pub fn render(&mut self, notices: &mut Vec<Notice>) {
        if self.dirty {
            self.sort();
        }
        let (frame, rate, frames) = (self.frame, self.rate, self.quantum);
        let mut dead = Vec::new();
        for position in 0..self.order.len() {
            let step = self.order[position];
            let (Step::Node(index) | Step::Read(index) | Step::Write(index)) = step;
            let Some(mut node) = self.nodes[index].take() else {
                continue;
            };
            if let Step::Write(_) = step {
                let inputs_active = Self::mix_inputs(&self.nodes, &mut node, frames);
                let params = Params {
                    slab: &self.params,
                    ids: &node.params,
                };
                let mut scope = Scope {
                    frame,
                    rate,
                    frames,
                    node: node.id,
                    inputs_active,
                    connected: node.connected,
                    notices,
                };
                node.processor.write(&mut scope, &node.input_buses, &params);
                self.nodes[index] = Some(node);
                continue;
            }
            for &param in &node.params {
                Self::compute_param(
                    &mut self.params,
                    &self.nodes,
                    &mut self.mono,
                    param,
                    frame,
                    rate,
                    frames,
                );
            }
            let inputs_active = matches!(step, Step::Node(_))
                && !node.muted
                && Self::mix_inputs(&self.nodes, &mut node, frames);
            let source = node.processor.is_source();
            if !node.muted && (step != Step::Node(index) || source || inputs_active || node.tail) {
                let params = Params {
                    slab: &self.params,
                    ids: &node.params,
                };
                let mut scope = Scope {
                    frame,
                    rate,
                    frames,
                    node: node.id,
                    inputs_active,
                    connected: node.connected,
                    notices,
                };
                if let Step::Read(_) = step {
                    node.active = node.processor.read(&mut scope, &mut node.outputs, &params);
                    node.tail = false;
                } else {
                    let result = node.processor.process(
                        &mut scope,
                        &node.input_buses,
                        &mut node.outputs,
                        &params,
                    );
                    if source {
                        node.active = result;
                        node.tail = false;
                    } else {
                        node.active = true;
                        node.tail = result;
                    }
                }
            } else {
                node.active = false;
                node.tail = false;
            }
            if !node.active {
                for output in &mut node.outputs {
                    output.reset(1, frames);
                }
                if node.released && Some(index) != self.destination {
                    dead.push(index);
                }
            }
            self.nodes[index] = Some(node);
        }
        for index in dead {
            self.remove(index);
        }
        self.frame += frames as u64;
    }
}

/// Strongly connected components of the "depends on" graph (iterative
/// Tarjan), each after the components it depends on, with whether it is a
/// cycle.
fn components(present: &[bool], depends: &[Vec<usize>]) -> Vec<(Vec<usize>, bool)> {
    const UNSEEN: usize = usize::MAX;
    let count = present.len();
    let mut number = vec![UNSEEN; count];
    let mut low = vec![0; count];
    let mut on_stack = vec![false; count];
    let mut stack = Vec::new();
    let mut calls: Vec<(usize, usize)> = Vec::new();
    let mut next = 0;
    let mut result = Vec::new();
    for root in 0..count {
        if !present[root] || number[root] != UNSEEN {
            continue;
        }
        calls.push((root, 0));
        number[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (vertex, ref mut child)) = calls.last_mut() {
            if let Some(&dependency) = depends[vertex].get(*child) {
                *child += 1;
                if !present[dependency] {
                    continue;
                }
                if number[dependency] == UNSEEN {
                    number[dependency] = next;
                    low[dependency] = next;
                    next += 1;
                    stack.push(dependency);
                    on_stack[dependency] = true;
                    calls.push((dependency, 0));
                } else if on_stack[dependency] {
                    low[vertex] = low[vertex].min(number[dependency]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                low[parent] = low[parent].min(low[vertex]);
            }
            if low[vertex] == number[vertex] {
                let start = stack
                    .iter()
                    .rposition(|&member| member == vertex)
                    .expect("on stack");
                let component: Vec<usize> = stack.drain(start..).collect();
                let cycle = component.len() > 1 || depends[vertex].contains(&vertex);
                for &member in &component {
                    on_stack[member] = false;
                }
                result.push((component, cycle));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webaudio::bus::CountMode;
    use crate::webaudio::nodes::{Destination, Gain, Oscillator};
    use crate::webaudio::param::Rate;
    use crate::webaudio::wave::{Builtin, Wave};

    fn config(count: usize, mode: CountMode) -> ChannelConfig {
        ChannelConfig {
            count,
            mode,
            interpretation: Interpretation::Speakers,
        }
    }

    fn descriptor(default: f32) -> Descriptor {
        Descriptor {
            default,
            min: f32::MIN,
            max: f32::MAX,
            rate: Rate::A,
            compound: false,
        }
    }

    fn graph() -> Graph {
        let mut graph = Graph::new(8000.0, 128);
        graph.apply(GraphMessage::Create {
            id: 1,
            init: Box::new(NodeInit {
                processor: Box::new(Destination),
                inputs: 1,
                outputs: 1,
                channel: config(2, CountMode::Explicit),
                params: Vec::new(),
            }),
        });
        graph
    }

    fn gain(graph: &mut Graph, id: u32, value: f32) {
        graph.apply(GraphMessage::Create {
            id,
            init: Box::new(NodeInit {
                processor: Box::new(Gain),
                inputs: 1,
                outputs: 1,
                channel: config(2, CountMode::Max),
                params: vec![(id + 1, descriptor(value), Arc::new(AtomicU32::new(0)))],
            }),
        });
    }

    fn oscillator(graph: &mut Graph, id: u32) {
        let compound = Descriptor {
            compound: true,
            ..descriptor(440.0)
        };
        graph.apply(GraphMessage::Create {
            id,
            init: Box::new(NodeInit {
                processor: Box::new(Oscillator::new(Wave::builtin(Builtin::Square))),
                inputs: 0,
                outputs: 1,
                channel: config(2, CountMode::Max),
                params: vec![
                    (id + 1, compound, Arc::new(AtomicU32::new(0))),
                    (
                        id + 2,
                        Descriptor {
                            default: 0.0,
                            ..compound
                        },
                        Arc::new(AtomicU32::new(0)),
                    ),
                ],
            }),
        });
    }

    #[test]
    fn sources_play_through_gain_to_the_destination_and_end() {
        let mut graph = graph();
        oscillator(&mut graph, 10);
        gain(&mut graph, 20, 0.5);
        graph.apply(GraphMessage::Connect {
            from: 10,
            output: 0,
            to: 20,
            input: 0,
        });
        graph.apply(GraphMessage::Connect {
            from: 20,
            output: 0,
            to: 1,
            input: 0,
        });
        let mut notices = Vec::new();
        graph.render(&mut notices);
        assert!(graph.destination_output().unwrap().is_silent());
        graph.apply(GraphMessage::Control {
            node: 10,
            control: Control::Start {
                when: 0.0,
                offset: 0.0,
                duration: None,
            },
        });
        graph.apply(GraphMessage::Control {
            node: 10,
            control: Control::Stop(0.04),
        });
        graph.render(&mut notices);
        let output = graph.destination_output().unwrap();
        assert_eq!(output.channels(), 2);
        let peak = output
            .channel(1)
            .iter()
            .fold(0f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak > 0.4 && peak < 0.6, "{peak}");
        graph.render(&mut notices);
        graph.render(&mut notices);
        assert!(matches!(notices[..], [Notice::Ended(10)]));
        // Released silent nodes leave the graph.
        graph.apply(GraphMessage::Release(10));
        graph.apply(GraphMessage::Release(20));
        graph.render(&mut notices);
        assert_eq!(graph.node_count(), 1);
    }

    #[test]
    fn cycles_without_a_delay_are_muted() {
        let mut graph = graph();
        oscillator(&mut graph, 10);
        gain(&mut graph, 20, 1.0);
        gain(&mut graph, 30, 1.0);
        for (from, to) in [(10, 20), (20, 30), (30, 20), (30, 1)] {
            graph.apply(GraphMessage::Connect {
                from,
                output: 0,
                to,
                input: 0,
            });
        }
        graph.apply(GraphMessage::Control {
            node: 10,
            control: Control::Start {
                when: 0.0,
                offset: 0.0,
                duration: None,
            },
        });
        let mut notices = Vec::new();
        graph.render(&mut notices);
        assert!(graph.destination_output().unwrap().is_silent());
        graph.apply(GraphMessage::Disconnect {
            from: 30,
            output: 0,
            to: 20,
            input: 0,
        });
        graph.render(&mut notices);
        assert!(!graph.destination_output().unwrap().is_silent());
    }
}
