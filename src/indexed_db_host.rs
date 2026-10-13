//! Indexed Database 3 (local w3c/IndexedDB@f704918, 2026-09-06): the database
//! coordinator of one storage shed (Storage #storage-shed).
//!
//! Databases belong to a storage key, not to a realm: every Window, frame and
//! worker of an origin opens the same databases. The standard keeps what must
//! therefore be shared outside any event loop, and this coordinator owns it
//! for every agent of the shed:
//!
//! * the connection queues of #connection-queues, which run each storage key
//!   and name's open/delete requests one after another;
//! * the connections of each database with their close pending flags, so an
//!   upgrade or deletion fires `versionchange` at connections in other agents,
//!   waits for those events, fires `blocked` while any stays open and
//!   continues once all are closed (#opening steps 10.1-10.5 and
//!   #deleting-a-database steps 5-9);
//! * transaction scheduling (#transaction-scheduling): transactions are
//!   ordered by creation across agents, and one starts only when no earlier
//!   unfinished transaction with an overlapping scope conflicts with it.
//!
//! Each step that the standard performs by queueing a database task becomes a
//! task in the inbox of the realm owning its request, connection or
//! transaction; the realm's event loop takes it like a port message (it is
//! woken the same way), so no agent polls. The committed data itself stays in
//! the storage shed's `indexed-database:` bottles, written by the platform
//! prelude. An agent or realm that ends releases everything it owned: its
//! connections are closed (#database-connection: "if the execution context
//! where the connection was created is destroyed ... the connection is
//! closed"), its transactions finish uncommitted and its queued requests leave
//! their queues.

use super::message_port_host::Wake;
use super::{Ctx, HostState, TrustJsTaskSender, Value};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};

type Shed = Mutex<HashMap<String, HashMap<String, String>>>;

/// A database within a storage shed: its storage key and its name.
type DatabaseKey = (String, String);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Owner {
    agent: u64,
    realm: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum Task {
    /// #connection-queues: every previous request in the queue was processed.
    Run(u64),
    /// #fire-a-version-change-event named versionchange at a connection.
    VersionChange {
        connection: u64,
        request: u64,
        old: u64,
        new: Option<u64>,
    },
    /// #fire-a-version-change-event named blocked at an open request.
    Blocked {
        request: u64,
        old: u64,
        new: Option<u64>,
    },
    /// Every connection the request waited for is closed.
    Unblocked(u64),
    /// #transaction-lifecycle: the scheduling constraints are satisfied.
    Start(u64),
}

impl Task {
    fn concerns_request(&self, id: u64) -> bool {
        match self {
            Self::Run(request) | Self::Unblocked(request) => *request == id,
            Self::Blocked { request, .. } => *request == id,
            Self::VersionChange { .. } | Self::Start(_) => false,
        }
    }
}

struct Inbox {
    tasks: VecDeque<Task>,
    wake: Wake,
}

/// The versionchange/blocked handshake of an open or delete request.
struct Wait {
    /// openConnections that are not closed yet.
    open: BTreeSet<u64>,
    /// Connections whose versionchange event has not been fired yet.
    unfired: BTreeSet<u64>,
    blocked: bool,
    old: u64,
    new: Option<u64>,
}

struct Request {
    owner: Owner,
    database: DatabaseKey,
    wait: Option<Wait>,
}

struct Connection {
    owner: Owner,
    database: DatabaseKey,
    close_pending: bool,
}

struct Transaction {
    owner: Owner,
    database: DatabaseKey,
    scope: Vec<String>,
    read_write: bool,
    started: bool,
}

#[derive(Default)]
struct Database {
    /// The connection queue: open and delete requests, front first.
    queue: VecDeque<u64>,
    /// Connections that are not closed.
    connections: BTreeSet<u64>,
    /// Unfinished transactions in creation order.
    transactions: Vec<u64>,
}

impl Database {
    fn idle(&self) -> bool {
        self.queue.is_empty() && self.connections.is_empty() && self.transactions.is_empty()
    }
}

#[derive(Default)]
pub(super) struct Coordinator {
    databases: HashMap<DatabaseKey, Database>,
    requests: HashMap<u64, Request>,
    connections: HashMap<u64, Connection>,
    transactions: HashMap<u64, Transaction>,
    inboxes: HashMap<Owner, Inbox>,
    next_id: u64,
}

/// One coordinator per storage shed, found by the shed's identity. Entries of
/// sheds that no longer exist are dropped on the next lookup.
type ShedCoordinator = (Weak<Shed>, Arc<Mutex<Coordinator>>);
static COORDINATORS: Mutex<Vec<ShedCoordinator>> = Mutex::new(Vec::new());

fn coordinator_for(storage: &crate::js::WebStorage) -> Arc<Mutex<Coordinator>> {
    let mut list = COORDINATORS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    list.retain(|(shed, _)| shed.strong_count() > 0);
    if let Some((_, coordinator)) = list
        .iter()
        .find(|(shed, _)| std::ptr::eq(shed.as_ptr(), Arc::as_ptr(storage)))
    {
        return coordinator.clone();
    }
    let coordinator = Arc::<Mutex<Coordinator>>::default();
    list.push((Arc::downgrade(storage), coordinator.clone()));
    coordinator
}

/// An agent's identity towards the coordinator of its storage shed.
pub(super) struct Agent {
    id: u64,
    coordinator: Option<Arc<Mutex<Coordinator>>>,
}

impl Default for Agent {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            coordinator: None,
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.release(None);
    }
}

impl Agent {
    /// The agent (or one realm of it) ended: release what it owned.
    pub fn release(&self, realm: Option<u64>) {
        if let Some(coordinator) = &self.coordinator {
            coordinator
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .release(self.id, realm);
        }
    }
}

impl Coordinator {
    fn allocate(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn push(&mut self, owner: Owner, task: Task) {
        if let Some(inbox) = self.inboxes.get_mut(&owner) {
            let notify = inbox.tasks.is_empty();
            inbox.tasks.push_back(task);
            if notify {
                inbox.wake.notify();
            }
        }
    }

    fn purge(&mut self, matches: impl Fn(&Task) -> bool) {
        for inbox in self.inboxes.values_mut() {
            inbox.tasks.retain(|task| !matches(task));
        }
    }

    fn forget_if_idle(&mut self, database: &DatabaseKey) {
        if self.databases.get(database).is_some_and(Database::idle) {
            self.databases.remove(database);
        }
    }

    /// The request at the front of a connection queue may run.
    fn run_front(&mut self, database: &DatabaseKey) {
        let front = self
            .databases
            .get(database)
            .and_then(|database| database.queue.front().copied());
        if let Some(request) = front
            && let Some(owner) = self.requests.get(&request).map(|request| request.owner)
        {
            self.push(owner, Task::Run(request));
        }
    }

    /// #opening steps 10.3-10.5 (and #deleting-a-database steps 7-9): once
    /// every versionchange event was fired, `blocked` while any connection is
    /// still open, then continue when all are closed.
    fn evaluate(&mut self, request: u64) {
        let Some(entry) = self.requests.get_mut(&request) else {
            return;
        };
        let owner = entry.owner;
        let Some(wait) = entry.wait.as_mut() else {
            return;
        };
        if !wait.unfired.is_empty() {
            return;
        }
        if wait.open.is_empty() {
            entry.wait = None;
            self.push(owner, Task::Unblocked(request));
        } else if !wait.blocked {
            wait.blocked = true;
            let (old, new) = (wait.old, wait.new);
            self.push(owner, Task::Blocked { request, old, new });
        }
    }

    /// #transaction-scheduling over a database's unfinished transactions.
    fn schedule(&mut self, database: &DatabaseKey) {
        let Some(list) = self
            .databases
            .get(database)
            .map(|database| database.transactions.clone())
        else {
            return;
        };
        for (index, &id) in list.iter().enumerate() {
            let Some(transaction) = self.transactions.get(&id) else {
                continue;
            };
            if transaction.started {
                continue;
            }
            let blocked = list[..index].iter().any(|prior| {
                self.transactions.get(prior).is_some_and(|prior| {
                    (transaction.read_write || prior.read_write)
                        && prior
                            .scope
                            .iter()
                            .any(|name| transaction.scope.contains(name))
                })
            });
            if !blocked {
                let owner = transaction.owner;
                if let Some(transaction) = self.transactions.get_mut(&id) {
                    transaction.started = true;
                }
                self.push(owner, Task::Start(id));
            }
        }
    }

    fn connection_closed(&mut self, id: u64) {
        let Some(connection) = self.connections.remove(&id) else {
            return;
        };
        if let Some(database) = self.databases.get_mut(&connection.database) {
            database.connections.remove(&id);
        }
        self.purge(
            |task| matches!(task, Task::VersionChange { connection, .. } if *connection == id),
        );
        let waiting: Vec<u64> = self
            .requests
            .iter_mut()
            .filter_map(|(&request, entry)| {
                let wait = entry.wait.as_mut()?;
                let affected = wait.open.remove(&id) | wait.unfired.remove(&id);
                affected.then_some(request)
            })
            .collect();
        for request in waiting {
            self.evaluate(request);
        }
        self.forget_if_idle(&connection.database);
    }

    fn transaction_finished(&mut self, id: u64) {
        let Some(transaction) = self.transactions.remove(&id) else {
            return;
        };
        if let Some(database) = self.databases.get_mut(&transaction.database) {
            database.transactions.retain(|other| *other != id);
        }
        self.purge(|task| *task == Task::Start(id));
        self.schedule(&transaction.database);
        self.forget_if_idle(&transaction.database);
    }

    fn request_finished(&mut self, id: u64) {
        let Some(request) = self.requests.remove(&id) else {
            return;
        };
        self.purge(|task| task.concerns_request(id));
        let mut was_front = false;
        if let Some(database) = self.databases.get_mut(&request.database) {
            was_front = database.queue.front() == Some(&id);
            database.queue.retain(|other| *other != id);
        }
        if was_front {
            self.run_front(&request.database);
        }
        self.forget_if_idle(&request.database);
    }

    fn release(&mut self, agent: u64, realm: Option<u64>) {
        let owned =
            |owner: &Owner| owner.agent == agent && realm.is_none_or(|realm| owner.realm == realm);
        let transactions: Vec<u64> = self
            .transactions
            .iter()
            .filter(|(_, transaction)| owned(&transaction.owner))
            .map(|(&id, _)| id)
            .collect();
        let connections: Vec<u64> = self
            .connections
            .iter()
            .filter(|(_, connection)| owned(&connection.owner))
            .map(|(&id, _)| id)
            .collect();
        let requests: Vec<u64> = self
            .requests
            .iter()
            .filter(|(_, request)| owned(&request.owner))
            .map(|(&id, _)| id)
            .collect();
        self.inboxes.retain(|owner, _| !owned(owner));
        for id in transactions {
            self.transaction_finished(id);
        }
        for id in connections {
            self.connection_closed(id);
        }
        for id in requests {
            self.request_finished(id);
        }
    }

    fn task_value(ctx: &mut Ctx, task: Task) -> Value {
        let version = |version: Option<u64>| version.map_or(Value::Null, |v| Value::Num(v as f64));
        let id = |id: u64| Value::Num(id as f64);
        let name = |name: &str| Value::from_string(String::from(name));
        let items = match task {
            Task::Run(request) => vec![name("run"), id(request)],
            Task::VersionChange {
                connection,
                request,
                old,
                new,
            } => vec![
                name("versionchange"),
                id(connection),
                id(request),
                id(old),
                version(new),
            ],
            Task::Blocked { request, old, new } => {
                vec![name("blocked"), id(request), id(old), version(new)]
            }
            Task::Unblocked(request) => vec![name("unblocked"), id(request)],
            Task::Start(transaction) => vec![name("start"), id(transaction)],
        };
        ctx.make_array(items)
    }
}

fn item(ctx: &mut Ctx, list: &Value, index: usize) -> Value {
    ctx.member_get(list, &index.to_string())
        .unwrap_or(Value::Undefined)
}

fn item_id(ctx: &mut Ctx, list: &Value, index: usize) -> u64 {
    item(ctx, list, index)
        .as_num_opt()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map_or(0, |value| value as u64)
}

fn item_version(ctx: &mut Ctx, list: &Value, index: usize) -> Option<u64> {
    item(ctx, list, index)
        .as_num_opt()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as u64)
}

fn item_text(ctx: &mut Ctx, list: &Value, index: usize) -> String {
    let value = item(ctx, list, index);
    ctx.coerce_engine_text(&value)
        .map(|text| text.to_string())
        .unwrap_or_default()
}

fn item_texts(ctx: &mut Ctx, list: &Value, index: usize) -> Vec<String> {
    let value = item(ctx, list, index);
    let length = ctx
        .member_get(&value, "length")
        .ok()
        .and_then(|length| length.as_num_opt())
        .filter(|length| length.is_finite() && *length >= 0.0)
        .map_or(0, |length| length as usize);
    (0..length).map(|at| item_text(ctx, &value, at)).collect()
}

/// `__indexed_db(op, arguments)`: the platform prelude's coordinator operations.
/// The bootstrap captures this binding; author code never reaches it.
pub(super) fn call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let op = super::host_arg_string(ctx, args, 0);
    let list = args.get(1).cloned().unwrap_or(Value::Undefined);
    let realm = ctx.host_job_context();
    let (coordinator, agent, wake) = {
        let Some(state) = ctx.host_mut::<HostState>() else {
            return Ok(Value::Null);
        };
        let wake = match &state.task_events {
            Some(TrustJsTaskSender::Page(sender)) => Wake::Page(sender.downgrade()),
            Some(TrustJsTaskSender::Worker(inbox)) => Wake::Worker(inbox.clone()),
            None => state.message_ports.wake.clone(),
        };
        let storage = state.storage.clone();
        let agent = &mut state.indexed_db;
        let coordinator = agent
            .coordinator
            .get_or_insert_with(|| coordinator_for(&storage))
            .clone();
        (coordinator, agent.id, wake)
    };
    let owner = Owner { agent, realm };
    // Read every argument before taking the coordinator's lock.
    let numbers: Vec<u64> = match op.as_str() {
        "finish" | "connect" | "closepending" | "closed" | "finished" => {
            vec![item_id(ctx, &list, 0)]
        }
        "ack" | "versionchange" | "transaction" => {
            vec![item_id(ctx, &list, 0), item_id(ctx, &list, 1)]
        }
        _ => Vec::new(),
    };
    let texts = match op.as_str() {
        "request" => vec![item_text(ctx, &list, 0), item_text(ctx, &list, 1)],
        "transaction" => item_texts(ctx, &list, 1),
        _ => Vec::new(),
    };
    let versions = if op == "versionchange" {
        (
            item_version(ctx, &list, 2).unwrap_or(0),
            item_version(ctx, &list, 3),
        )
    } else {
        (0, None)
    };
    let read_write = if op == "transaction" {
        let mode = item(ctx, &list, 2);
        ctx.to_boolean(&mode)
    } else {
        false
    };
    let mut guard = coordinator
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let coordinator = &mut *guard;
    let inbox = coordinator.inboxes.entry(owner).or_insert_with(|| Inbox {
        tasks: VecDeque::new(),
        wake: Wake::None,
    });
    inbox.wake = wake;
    let owns_request = |coordinator: &Coordinator, id: u64| {
        coordinator
            .requests
            .get(&id)
            .is_some_and(|request| request.owner.agent == owner.agent)
    };
    let owns_connection = |coordinator: &Coordinator, id: u64| {
        coordinator
            .connections
            .get(&id)
            .is_some_and(|connection| connection.owner.agent == owner.agent)
    };
    let result = match op.as_str() {
        "has" => Value::Bool(
            coordinator
                .inboxes
                .get(&owner)
                .is_some_and(|inbox| !inbox.tasks.is_empty()),
        ),
        "take" => {
            let task = coordinator
                .inboxes
                .get_mut(&owner)
                .and_then(|inbox| inbox.tasks.pop_front());
            drop(guard);
            return Ok(task.map_or(Value::Null, |task| Coordinator::task_value(ctx, task)));
        }
        // #connection-queues: add the request to its queue.
        "request" => {
            let mut texts = texts.into_iter();
            let database = (
                texts.next().unwrap_or_default(),
                texts.next().unwrap_or_default(),
            );
            let id = coordinator.allocate();
            let queue = &mut coordinator
                .databases
                .entry(database.clone())
                .or_default()
                .queue;
            queue.push_back(id);
            let front = queue.len() == 1;
            coordinator.requests.insert(
                id,
                Request {
                    owner,
                    database,
                    wait: None,
                },
            );
            if front {
                coordinator.push(owner, Task::Run(id));
            }
            Value::Num(id as f64)
        }
        // The request was processed: the next request in its queue may run.
        "finish" => {
            if owns_request(coordinator, numbers[0]) {
                coordinator.request_finished(numbers[0]);
            }
            Value::Undefined
        }
        // #opening step 8: a new connection to the request's database.
        "connect" => match coordinator.requests.get(&numbers[0]) {
            Some(request) if request.owner.agent == owner.agent => {
                let database = request.database.clone();
                let id = coordinator.allocate();
                coordinator
                    .databases
                    .entry(database.clone())
                    .or_default()
                    .connections
                    .insert(id);
                coordinator.connections.insert(
                    id,
                    Connection {
                        owner,
                        database,
                        close_pending: false,
                    },
                );
                Value::Num(id as f64)
            }
            _ => Value::Num(0.0),
        },
        // #opening step 10.1-10.2 / #deleting-a-database steps 5-6:
        // versionchange at every other connection without close pending.
        "versionchange" => {
            let (request, excluded) = (numbers[0], numbers[1]);
            let (old, new) = versions;
            if owns_request(coordinator, request) {
                let database = coordinator.requests[&request].database.clone();
                let open: BTreeSet<u64> = coordinator
                    .databases
                    .get(&database)
                    .map(|database| {
                        database
                            .connections
                            .iter()
                            .copied()
                            .filter(|connection| *connection != excluded)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut unfired = BTreeSet::new();
                for &connection in &open {
                    let entry = &coordinator.connections[&connection];
                    if entry.close_pending {
                        continue;
                    }
                    unfired.insert(connection);
                    let target = entry.owner;
                    coordinator.push(
                        target,
                        Task::VersionChange {
                            connection,
                            request,
                            old,
                            new,
                        },
                    );
                }
                if let Some(entry) = coordinator.requests.get_mut(&request) {
                    entry.wait = Some(Wait {
                        open,
                        unfired,
                        blocked: false,
                        old,
                        new,
                    });
                }
                coordinator.evaluate(request);
            }
            Value::Undefined
        }
        // "Wait for all of the events to be fired."
        "ack" => {
            let (connection, request) = (numbers[0], numbers[1]);
            let fired = coordinator
                .connections
                .get(&connection)
                .is_none_or(|entry| entry.owner.agent == owner.agent);
            if fired
                && let Some(wait) = coordinator
                    .requests
                    .get_mut(&request)
                    .and_then(|entry| entry.wait.as_mut())
                && wait.unfired.remove(&connection)
            {
                coordinator.evaluate(request);
            }
            Value::Undefined
        }
        "closepending" => {
            if owns_connection(coordinator, numbers[0])
                && let Some(connection) = coordinator.connections.get_mut(&numbers[0])
            {
                connection.close_pending = true;
            }
            Value::Undefined
        }
        // #close-a-database-connection step 3: the connection is closed.
        "closed" => {
            if owns_connection(coordinator, numbers[0]) {
                coordinator.connection_closed(numbers[0]);
            }
            Value::Undefined
        }
        // #transaction-lifecycle step 1: created, ordered after every
        // transaction created before it in any agent.
        "transaction" => match coordinator.connections.get(&numbers[0]) {
            Some(connection) if connection.owner.agent == owner.agent => {
                let database = connection.database.clone();
                let id = coordinator.allocate();
                coordinator.transactions.insert(
                    id,
                    Transaction {
                        owner,
                        database: database.clone(),
                        scope: texts,
                        read_write,
                        started: false,
                    },
                );
                coordinator
                    .databases
                    .entry(database.clone())
                    .or_default()
                    .transactions
                    .push(id);
                coordinator.schedule(&database);
                Value::Num(id as f64)
            }
            _ => Value::Num(0.0),
        },
        "finished" => {
            if coordinator
                .transactions
                .get(&numbers[0])
                .is_some_and(|transaction| transaction.owner.agent == owner.agent)
            {
                coordinator.transaction_finished(numbers[0]);
            }
            Value::Undefined
        }
        _ => Value::Undefined,
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(agent: u64) -> Owner {
        Owner { agent, realm: 0 }
    }

    fn coordinator_with(owners: &[Owner]) -> Coordinator {
        let mut coordinator = Coordinator::default();
        for &owner in owners {
            coordinator.inboxes.insert(
                owner,
                Inbox {
                    tasks: VecDeque::new(),
                    wake: Wake::None,
                },
            );
        }
        coordinator
    }

    fn take(coordinator: &mut Coordinator, owner: Owner) -> Vec<Task> {
        coordinator
            .inboxes
            .get_mut(&owner)
            .map(|inbox| inbox.tasks.drain(..).collect())
            .unwrap_or_default()
    }

    fn transaction(coordinator: &mut Coordinator, owner: Owner, scope: &[&str], rw: bool) -> u64 {
        let database = (String::from("https://a.test"), String::from("db"));
        let id = coordinator.allocate();
        coordinator.transactions.insert(
            id,
            Transaction {
                owner,
                database: database.clone(),
                scope: scope.iter().map(|name| String::from(*name)).collect(),
                read_write: rw,
                started: false,
            },
        );
        coordinator
            .databases
            .entry(database.clone())
            .or_default()
            .transactions
            .push(id);
        coordinator.schedule(&database);
        id
    }

    #[test]
    fn transactions_start_in_creation_order_across_agents() {
        // IndexedDB #transaction-scheduling: a read-only transaction waits only
        // for earlier overlapping read/write transactions; a read/write one
        // waits for every earlier overlapping transaction, whatever its agent.
        let (a, b) = (owner(1), owner(2));
        let mut coordinator = coordinator_with(&[a, b]);
        let first = transaction(&mut coordinator, a, &["x", "y"], true);
        let reader = transaction(&mut coordinator, b, &["y"], false);
        let other = transaction(&mut coordinator, b, &["z"], true);
        let second_reader = transaction(&mut coordinator, a, &["z"], false);
        assert_eq!(take(&mut coordinator, a), [Task::Start(first)]);
        assert_eq!(take(&mut coordinator, b), [Task::Start(other)]);
        coordinator.transaction_finished(first);
        assert_eq!(take(&mut coordinator, b), [Task::Start(reader)]);
        assert!(take(&mut coordinator, a).is_empty());
        coordinator.transaction_finished(other);
        assert_eq!(take(&mut coordinator, a), [Task::Start(second_reader)]);
        coordinator.transaction_finished(reader);
        coordinator.transaction_finished(second_reader);
        assert!(
            coordinator.databases.is_empty(),
            "idle databases are forgotten"
        );
    }

    #[test]
    fn released_agents_close_connections_and_leave_connection_queues() {
        // #database-connection: a destroyed context's connections are closed,
        // which unblocks a waiting upgrade; its queued requests are dropped.
        let (a, b) = (owner(1), owner(2));
        let mut coordinator = coordinator_with(&[a, b]);
        let database = (String::from("https://a.test"), String::from("db"));
        let connection = coordinator.allocate();
        coordinator
            .databases
            .entry(database.clone())
            .or_default()
            .connections
            .insert(connection);
        coordinator.connections.insert(
            connection,
            Connection {
                owner: a,
                database: database.clone(),
                close_pending: false,
            },
        );
        let request = coordinator.allocate();
        let queued = coordinator.allocate();
        coordinator
            .databases
            .get_mut(&database)
            .unwrap()
            .queue
            .extend([request, queued]);
        for (id, by) in [(request, b), (queued, a)] {
            coordinator.requests.insert(
                id,
                Request {
                    owner: by,
                    database: database.clone(),
                    wait: None,
                },
            );
        }
        coordinator.requests.get_mut(&request).unwrap().wait = Some(Wait {
            open: BTreeSet::from([connection]),
            unfired: BTreeSet::from([connection]),
            blocked: false,
            old: 1,
            new: Some(2),
        });
        coordinator.release(1, None);
        assert_eq!(take(&mut coordinator, b), [Task::Unblocked(request)]);
        assert_eq!(
            coordinator.databases[&database].queue,
            VecDeque::from([request])
        );
        coordinator.request_finished(request);
        assert!(coordinator.databases.is_empty());
    }
}
