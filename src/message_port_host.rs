//! HTML §9.4.4 port endpoints outlive their realm-specific wrappers. A transfer moves
//! ownership of the queue, not its messages, and the receiving queue starts disabled.
//! Local WHATWG HTML snapshot e5071a20 (2026-09-06), #transferMessagePort.

use super::{Ctx, HostState, LumenHostTask, LumenWorkerCtl, Value, host_arg_string};
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};

const MAX_PORTS: usize = 16_384;
const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Default)]
pub(super) enum Wake {
    #[default]
    None,
    Page(tokio::sync::mpsc::WeakUnboundedSender<LumenHostTask>),
    Worker(Weak<std::sync::mpsc::SyncSender<LumenWorkerCtl>>),
}

impl Wake {
    fn notify(&self) {
        match self {
            Self::None => {}
            Self::Page(sender) => {
                if let Some(sender) = sender.upgrade() {
                    let _ = sender.send(LumenHostTask::PortReady);
                }
            }
            Self::Worker(sender) => {
                if let Some(sender) = sender.upgrade() {
                    // A full inbox already wakes the worker. Its loop checks port readiness
                    // between commands, so a redundant wake may safely be coalesced.
                    let _ = sender.try_send(LumenWorkerCtl::PortReady);
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Owner {
    agent: u64,
    realm: u64,
}

struct Endpoint {
    owner: Option<Owner>,
    transfer_key: Option<String>,
    wake: Wake,
    peer: Option<u64>,
    enabled: bool,
    messages: VecDeque<(u64, String)>,
}

#[derive(Default)]
pub(super) struct Registry {
    ports: HashMap<u64, Endpoint>,
    next_id: u64,
    sequence: u64,
    queued_bytes: usize,
}

pub(super) struct Agent {
    pub registry: Arc<Mutex<Registry>>,
    pub wake: Wake,
    id: u64,
}

impl Default for Agent {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            registry: Arc::default(),
            wake: Wake::None,
            id: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.release(None);
    }
}

impl Agent {
    pub fn release(&self, realm: Option<u64>) {
        let mut registry = self.registry.lock().unwrap();
        let ids: Vec<_> = registry
            .ports
            .iter()
            .filter_map(|(&id, port)| {
                port.owner
                    .filter(|owner| {
                        owner.agent == self.id && realm.is_none_or(|realm| owner.realm == realm)
                    })
                    .map(|_| id)
            })
            .collect();
        for id in ids {
            if let Some(port) = registry.ports.remove(&id) {
                registry.queued_bytes -= port
                    .messages
                    .iter()
                    .map(|(_, message)| message.len())
                    .sum::<usize>();
                if let Some(peer) = port.peer.and_then(|peer| registry.ports.get_mut(&peer)) {
                    peer.peer = None;
                }
            }
        }
    }

    pub fn scan_retained_memory(&self, visitor: &mut dyn lumen::embed::HostRetainedMemoryVisitor) {
        let registry = self.registry.lock().unwrap();
        let bytes = registry.ports.capacity() * std::mem::size_of::<(u64, Endpoint)>()
            + registry
                .ports
                .values()
                .map(|port| {
                    port.messages.capacity() * std::mem::size_of::<(u64, String)>()
                        + port
                            .messages
                            .iter()
                            .map(|(_, message)| message.capacity())
                            .sum::<usize>()
                })
                .sum::<usize>();
        visitor.allocation(lumen::embed::RetainedManagedAllocation::new(
            "trust.message-port-queues",
            Arc::as_ptr(&self.registry) as usize,
            bytes,
        ));
    }
}

impl Registry {
    fn ready(&self, owner: Owner) -> Option<u64> {
        self.ports
            .iter()
            .filter(|(_, port)| port.owner == Some(owner) && port.enabled)
            .filter_map(|(&id, port)| port.messages.front().map(|(sequence, _)| (id, *sequence)))
            .min_by_key(|(_, sequence)| *sequence)
            .map(|(id, _)| id)
    }

    fn operation(
        &mut self,
        owner: Owner,
        wake: Wake,
        op: &str,
        id: u64,
        payload: String,
    ) -> Result<serde_json::Value, &'static str> {
        use serde_json::json;
        match op {
            "create" => {
                if self.ports.len() + 2 > MAX_PORTS {
                    return Err("MessagePort limit exceeded");
                }
                let a = self.next_id + 1;
                let b = a + 1;
                self.next_id = b;
                for (id, peer) in [(a, b), (b, a)] {
                    self.ports.insert(
                        id,
                        Endpoint {
                            owner: Some(owner),
                            transfer_key: None,
                            wake: wake.clone(),
                            peer: Some(peer),
                            enabled: false,
                            messages: VecDeque::new(),
                        },
                    );
                }
                Ok(json!([a, b]))
            }
            "has" => Ok(json!(self.ready(owner).is_some())),
            "take" => {
                let Some(id) = self.ready(owner) else {
                    return Ok(serde_json::Value::Null);
                };
                let (_, message) = self
                    .ports
                    .get_mut(&id)
                    .unwrap()
                    .messages
                    .pop_front()
                    .unwrap();
                self.queued_bytes -= message.len();
                Ok(json!([id, message]))
            }
            "send" => {
                let Some(peer) = self.ports.get_mut(&id) else {
                    return Ok(json!(true));
                };
                if payload.len() > MAX_QUEUED_BYTES.saturating_sub(self.queued_bytes) {
                    return Err("MessagePort queue limit exceeded");
                }
                self.queued_bytes += payload.len();
                self.sequence += 1;
                let notify = peer.enabled && peer.messages.is_empty();
                peer.messages.push_back((self.sequence, payload));
                if notify {
                    peer.wake.notify();
                }
                Ok(json!(true))
            }
            "receive" => {
                // Wire transfers carry an unguessable, single-use capability. An author
                // must not claim another Realm's in-flight endpoint by guessing its ID.
                let Some((&id, port)) = self.ports.iter_mut().find(|(_, port)| {
                    port.owner.is_none() && port.transfer_key.as_ref() == Some(&payload)
                }) else {
                    return Ok(json!(false));
                };
                port.transfer_key = None;
                port.owner = Some(owner);
                port.wake = wake;
                port.enabled = false;
                Ok(json!(id))
            }
            _ => {
                let Some(port) = self
                    .ports
                    .get_mut(&id)
                    .filter(|port| port.owner.is_some_and(|held| held.agent == owner.agent))
                else {
                    return Ok(json!(false));
                };
                match op {
                    "peer" => Ok(json!(port.peer)),
                    "transfer" => {
                        let mut random = [0u8; 16];
                        getrandom::fill(&mut random)
                            .map_err(|_| "MessagePort transfer allocation failed")?;
                        let key: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
                        port.transfer_key = Some(key.clone());
                        port.owner = None;
                        port.wake = Wake::None;
                        port.enabled = false;
                        Ok(json!(key))
                    }
                    "start" => {
                        let notify = !port.enabled && !port.messages.is_empty();
                        port.enabled = true;
                        if notify {
                            port.wake.notify();
                        }
                        Ok(json!(true))
                    }
                    "close" => {
                        let peer = port.peer.take();
                        if let Some(peer) = peer.and_then(|peer| self.ports.get_mut(&peer)) {
                            peer.peer = None;
                        }
                        Ok(json!(true))
                    }
                    _ => Ok(serde_json::Value::Null),
                }
            }
        }
    }
}

/// Bootstrap captures this binding; author code gets only branded MessagePort wrappers.
pub(super) fn call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let op = host_arg_string(ctx, args, 0);
    let id = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as u64;
    let payload = if op == "receive" {
        host_arg_string(ctx, args, 1)
    } else if op == "send" {
        host_arg_string(ctx, args, 2)
    } else {
        String::new()
    };
    let realm = ctx.host_job_context();
    let result = {
        let Some(state) = ctx.host_mut::<HostState>() else {
            return Ok(Value::from_string(String::from("null")));
        };
        let agent = &state.message_ports;
        let wake = state.task_events.as_ref().map_or_else(
            || agent.wake.clone(),
            |sender| Wake::Page(sender.downgrade()),
        );
        agent.registry.lock().unwrap().operation(
            Owner {
                agent: agent.id,
                realm,
            },
            wake,
            &op,
            id,
            payload,
        )
    };
    match result {
        Ok(value) => Ok(Value::from_string(value.to_string())),
        Err(message) => Err(ctx.make_error("QuotaExceededError", message)),
    }
}
