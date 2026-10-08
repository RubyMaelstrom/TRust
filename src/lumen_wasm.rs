//! WebAssembly JavaScript host boundary for the Lumen backend.
//!
//! The observable classes and promise APIs live in the shared platform prelude. This module owns
//! one wasmi store per JavaScript agent and translates the prelude's integer handles according to
//! the WebAssembly JavaScript Interface. A memory's bytes are one Data Block shared by Wasmi and
//! each fixed-length `Memory.buffer` (JS API #memories, "create a fixed length memory buffer"),
//! so writes on either side are visible to the other without copying. Growth detaches the
//! previous buffer at the next JS/wasm transition, before JavaScript can observe it
//! ("refresh the Memory buffer"); the replacement is created on the next `buffer` access.
//!
//! Exception handling (JS API #tags, #exceptions, #js-exceptions, "call an Exported Function"
//! step 11 and "create a host function" step 3.10; local snapshot of the spec repository
//! 37d6b059): tags are store objects identified by their address, so a tag that JavaScript
//! imports into several instances is one tag everywhere. A Wasm exception that leaves an
//! exported function becomes its cached `WebAssembly.Exception` object, or, for the
//! JavaScript exception tag, the original JavaScript value again. A JavaScript exception that
//! leaves an imported function becomes a Wasm exception that `catch`, `catch_all` and
//! `try_table` handlers catch: an `Exception` object throws its own exception address, any
//! other value is the `externref` payload of a fresh exception of the agent's JavaScript
//! exception tag. Host interruptions are no JavaScript exceptions and stay uncatchable traps.

use super::HostState;
use lumen::embed::{
    ArrayBufferBytes, Ctx, HostGc, HostGcVisitor, HostRetainedMemoryVisitor,
    RetainedExternalAllocation, RetainedExternalMemory, RetainedManagedAllocation, Value,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

static WASM_CALL_TRACE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    static EXTERNREF_ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(super) fn externref_allocations() -> usize {
    EXTERNREF_ALLOCATIONS.with(Cell::get)
}

/// `TRUST_WASM_TRACE`, read once: transitions run per call and must not take the environment lock.
fn trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TRUST_WASM_TRACE").is_some())
}

macro_rules! wasm_trace {
    ($message:expr) => {
        if trace_enabled() {
            eprintln!("wasm: {}", $message);
        }
    };
}

#[derive(Clone)]
pub(super) struct PageWasm {
    state: Rc<RefCell<Option<WasmState>>>,
    buffers: Rc<RefCell<Vec<Option<Value>>>>,
}

impl PageWasm {
    #[cfg(test)]
    pub(super) fn cached_functions(&self) -> usize {
        self.state
            .borrow()
            .as_ref()
            .map_or(0, |state| state.function_cache.len())
    }

    /// The numbers of cached Tag objects and cached Exception objects.
    #[cfg(test)]
    pub(super) fn cached_exception_objects(&self) -> (usize, usize) {
        self.state.borrow().as_ref().map_or((0, 0), |state| {
            (
                state.exceptions.tag_cache.len(),
                state.exceptions.exception_cache.len(),
            )
        })
    }

    pub(super) fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(None)),
            buffers: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(super) fn scan_retained_memory(&self, visitor: &mut dyn HostRetainedMemoryVisitor) {
        let mut state_bytes = std::mem::size_of_val(self.state.as_ref());
        match self.state.try_borrow() {
            Ok(state) => {
                if let Some(state) = state.as_ref() {
                    state_bytes = state_bytes.saturating_add(
                        state
                            .function_cache
                            .capacity()
                            .saturating_mul(std::mem::size_of::<(usize, Value)>()),
                    );
                    for value in state.function_cache.values() {
                        visitor.value(value);
                    }
                    let objects = &state.exceptions;
                    state_bytes = state_bytes
                        .saturating_add(
                            objects
                                .tags
                                .capacity()
                                .saturating_mul(std::mem::size_of::<wasmi::Tag>()),
                        )
                        .saturating_add(
                            objects
                                .tag_ids
                                .capacity()
                                .saturating_mul(std::mem::size_of::<(wasmi::Tag, usize)>()),
                        )
                        .saturating_add(
                            objects
                                .tag_cache
                                .capacity()
                                .saturating_mul(std::mem::size_of::<(usize, Value)>()),
                        )
                        .saturating_add(
                            objects
                                .exception_cache
                                .capacity()
                                .saturating_mul(std::mem::size_of::<(u32, Value)>()),
                        );
                    for value in objects
                        .tag_cache
                        .values()
                        .chain(objects.exception_cache.values())
                    {
                        visitor.value(value);
                    }
                    state_bytes =
                        state_bytes
                            .saturating_add(
                                state
                                    .modules
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<wasmi::Module>()),
                            )
                            .saturating_add(
                                state
                                    .instances
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<wasmi::Instance>()),
                            )
                            .saturating_add(
                                state
                                    .funcs
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<wasmi::Func>()),
                            )
                            .saturating_add(
                                state
                                    .globals
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<wasmi::Global>()),
                            )
                            .saturating_add(
                                state
                                    .memories
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<MemorySlot>()),
                            )
                            .saturating_add(
                                state
                                    .tables
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<wasmi::Table>()),
                            )
                            .saturating_add(
                                state
                                    .func_ids
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<(wasmi::Func, usize)>()),
                            )
                            .saturating_add(
                                state
                                    .global_ids
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<(wasmi::Global, usize)>()),
                            )
                            .saturating_add(
                                state
                                    .table_ids
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<(wasmi::Table, usize)>()),
                            )
                            .saturating_add(
                                state.store.data().externrefs.capacity().saturating_mul(
                                    std::mem::size_of::<(usize, wasmi::ExternRef)>(),
                                ),
                            );
                    // Wasmi's Engine/Store and compiled Module internals are private allocator
                    // graphs. Their visible handle/vector payload is retained above; classify
                    // the remainder as an opaque requested-payload lower bound, just like a
                    // standard-library map whose control-byte layout is private.
                    visitor.opaque_storage();
                }
            }
            Err(_) => visitor.unavailable(),
        }
        visitor.allocation(RetainedManagedAllocation::rc(
            "trust.wasmi-state",
            &self.state,
            state_bytes,
        ));

        let mut buffers_bytes = std::mem::size_of_val(self.buffers.as_ref());
        match self.buffers.try_borrow() {
            Ok(buffers) => {
                buffers_bytes = buffers_bytes.saturating_add(
                    buffers
                        .capacity()
                        .saturating_mul(std::mem::size_of::<Option<Value>>()),
                );
                for value in buffers.iter().flatten() {
                    visitor.value(value);
                }
            }
            Err(_) => visitor.unavailable(),
        }
        visitor.allocation(RetainedManagedAllocation::rc(
            "trust.wasmi-buffer-handles",
            &self.buffers,
            buffers_bytes,
        ));
    }
}

impl HostGc for PageWasm {
    fn trace_gc(&self, visitor: &mut dyn HostGcVisitor) {
        let Ok(state) = self.state.try_borrow() else {
            // Wasm may be suspended in an imported JS callback. Its operand stack and
            // in-flight instantiation are not represented by an idle Store snapshot.
            // Leave cache handles undiscounted (strong roots) for this collection.
            return;
        };
        let Some(state) = state.as_ref() else { return };
        trace_exception_objects(state, visitor);
        if state.function_cache.is_empty() {
            return;
        }

        // JS API #object-caches / #exported-function-exotic-objects: a wrapper's
        // identity remains observable through native funcrefs even without JS references.
        // ECMA-262 #sec-liveness permits reclaiming a whole unreachable group, not
        // rematerializing one member while another can return the original address.
        let mut groups = FunctionGroups::default();
        let mut imports = Vec::new();
        let mut roots = Vec::new();
        state.store.visit_function_references(
            |functions, imported| {
                groups.join(functions);
                if let Some(&owner) = functions.first() {
                    imports.extend(imported.iter().map(|&function| (owner, function)));
                }
            },
            |function| roots.push(function),
        );
        // An importer keeps its dependencies alive, never the reverse. Otherwise a
        // long-lived shared import would pin every throwaway importing instance.
        let mut links: HashMap<usize, Vec<usize>> = HashMap::new();
        for (owner, dependency) in imports {
            let owner = groups.group(owner);
            let dependency = groups.group(dependency);
            if owner != dependency {
                links.entry(owner).or_default().push(dependency);
            }
        }
        for targets in links.values_mut() {
            targets.sort_unstable();
            targets.dedup();
        }
        let mut representatives: HashMap<usize, &Value> = HashMap::new();
        for (&id, wrapper) in &state.function_cache {
            let Some(&function) = state.funcs.get(id) else {
                continue;
            };
            let group = groups.group(function);
            visitor.internal(wrapper);
            if let Some(first) = representatives.get(&group) {
                // A linear-size star, not a quadratic all-to-all export graph.
                visitor.edge(first, wrapper);
                visitor.edge(wrapper, first);
            } else {
                representatives.insert(group, wrapper);
            }
        }
        for (&group, &owner) in &representatives {
            if let Some(targets) = links.get(&group) {
                for &target in targets {
                    visit_cached_function_targets(target, &links, &representatives, &mut |value| {
                        visitor.edge(owner, value)
                    });
                }
            }
        }
        let root_groups: HashSet<usize> = roots
            .into_iter()
            .map(|function| groups.group(function))
            .collect();
        for group in root_groups {
            visit_cached_function_targets(group, &links, &representatives, &mut |value| {
                visitor.root(value)
            });
        }
    }

    fn sweep_gc(&mut self, is_live: &dyn Fn(&Value) -> bool) {
        let Ok(mut state) = self.state.try_borrow_mut() else {
            return;
        };
        if let Some(state) = state.as_mut() {
            state.function_cache.retain(|_, value| is_live(value));
            state.exceptions.tag_cache.retain(|_, value| is_live(value));
            // A collected Exception object releases its exception, which the store frees once
            // Wasm no longer references it either.
            let store = &state.store;
            state.exceptions.exception_cache.retain(|&address, value| {
                let live = is_live(value);
                if !live && let Some(exn) = wasmi::ExnRef::from_addr(store, address) {
                    exn.release(store);
                }
                live
            });
        }
    }
}

/// JS API #object-caches: the Tag and Exception object caches.
///
/// Both caches hold internal handles. A Tag object is only observable again through a
/// JavaScript reference to it, so its entry adds nothing else. An Exception object stays
/// observable while Wasm can still throw its exception out to JavaScript ("create an Exception
/// object" returns the cached object): it is a root while an `exnref` global or table, or a
/// field of such an exception, reaches its exception, and an Exception object whose exception's
/// fields reach another cached exception keeps that object alive as well.
fn trace_exception_objects(state: &WasmState, visitor: &mut dyn HostGcVisitor) {
    let objects = &state.exceptions;
    for wrapper in objects.tag_cache.values() {
        visitor.internal(wrapper);
    }
    if objects.exception_cache.is_empty() {
        return;
    }
    for wrapper in objects.exception_cache.values() {
        visitor.internal(wrapper);
    }
    let store = &state.store;
    let mut reachable = HashSet::new();
    let mut fields: HashMap<u32, Vec<u32>> = HashMap::new();
    store.visit_exception_references(
        |exn| {
            if let Some(address) = exn.addr(store) {
                reachable.insert(address);
            }
        },
        |from, to| {
            if let (Some(from), Some(to)) = (from.addr(store), to.addr(store)) {
                fields.entry(from).or_default().push(to);
            }
        },
    );
    for (address, wrapper) in &objects.exception_cache {
        if reachable.contains(address) {
            visitor.root(wrapper);
        }
        // Edges through uncached exceptions end at the next cached one, whose own outgoing
        // edges Lumen's mark queue follows.
        let mut pending = fields.get(address).cloned().unwrap_or_default();
        let mut seen = HashSet::new();
        while let Some(target) = pending.pop() {
            if !seen.insert(target) {
                continue;
            }
            match objects.exception_cache.get(&target) {
                Some(object) => visitor.edge(wrapper, object),
                None => pending.extend(fields.get(&target).into_iter().flatten()),
            }
        }
    }
}

/// Reach an exported wrapper through native-only intermediate modules as well. Stop at
/// another cached representative: its outgoing JS edges are handled by Lumen's mark queue.
fn visit_cached_function_targets(
    start: usize,
    links: &HashMap<usize, Vec<usize>>,
    representatives: &HashMap<usize, &Value>,
    visit: &mut impl FnMut(&Value),
) {
    let mut pending = vec![start];
    let mut seen = HashSet::new();
    while let Some(group) = pending.pop() {
        if !seen.insert(group) {
            continue;
        }
        if let Some(value) = representatives.get(&group) {
            visit(value);
        } else if let Some(targets) = links.get(&group) {
            pending.extend(targets);
        }
    }
}

/// Conservative components of functions DEFINED by each module instance. Imported
/// functions use directed dependencies, never membership in the importer's component.
#[derive(Default)]
struct FunctionGroups {
    indices: HashMap<wasmi::Func, usize>,
    parents: Vec<usize>,
    sizes: Vec<usize>,
}

impl FunctionGroups {
    fn group(&mut self, function: wasmi::Func) -> usize {
        let next = self.parents.len();
        let index = *self.indices.entry(function).or_insert_with(|| {
            self.parents.push(next);
            self.sizes.push(1);
            next
        });
        let mut root = index;
        while self.parents[root] != root {
            root = self.parents[root];
        }
        let mut current = index;
        while self.parents[current] != root {
            let next = self.parents[current];
            self.parents[current] = root;
            current = next;
        }
        root
    }

    fn join(&mut self, functions: &[wasmi::Func]) {
        let Some((&first, rest)) = functions.split_first() else {
            return;
        };
        let mut root = self.group(first);
        for &function in rest {
            let mut other = self.group(function);
            if other == root {
                continue;
            }
            if self.sizes[root] < self.sizes[other] {
                std::mem::swap(&mut root, &mut other);
            }
            self.parents[other] = root;
            self.sizes[root] += self.sizes[other];
        }
    }
}

impl RetainedExternalMemory for PageWasm {
    fn retained_external_memory(&self, visit: &mut dyn FnMut(RetainedExternalAllocation)) {
        let Ok(state) = self.state.try_borrow() else {
            return;
        };
        let Some(state) = state.as_ref() else {
            return;
        };
        for slot in &state.memories {
            // The Data Block identity lets Lumen count an exposed Memory.buffer only once.
            visit(RetainedExternalAllocation::wasm_array_buffer(&slot.storage));
        }
    }
}

struct MemorySlot {
    memory: wasmi::Memory,
    /// The memory's bytes, shared by Wasmi and every `Memory.buffer` of this memory.
    storage: ArrayBufferBytes,
    /// The memory size when its current `Memory.buffer` was created or last checked. A
    /// different size means the memory grew and that buffer must be detached.
    buffer_pages: u64,
}

#[derive(Default)]
struct StoreData {
    // JS API §5.6: repeated ToWebAssemblyValue conversions of the same host
    // value reuse its address. Keep this in Store data so re-entrant Caller
    // paths use the same cache without borrowing the active Store again.
    externrefs: HashMap<usize, wasmi::ExternRef>,
}

struct WasmState {
    engine: wasmi::Engine,
    store: wasmi::Store<StoreData>,
    modules: Vec<wasmi::Module>,
    instances: Vec<wasmi::Instance>,
    funcs: Vec<wasmi::Func>,
    /// Index of `funcs` (and of `globals` and `tables` below): Pyodide alone registers
    /// thousands of functions and GOT globals, and every funcref crossing looks one up.
    func_ids: HashMap<wasmi::Func, usize>,
    /// Strong handles accounted as internal cache edges by HostGc, not permanent roots.
    /// Native Store addresses themselves remain agent-lifetime allocations for now.
    function_cache: HashMap<usize, Value>,
    globals: Vec<wasmi::Global>,
    global_ids: HashMap<wasmi::Global, usize>,
    memories: Vec<MemorySlot>,
    tables: Vec<wasmi::Table>,
    table_ids: HashMap<wasmi::Table, usize>,
    /// Tags and the Tag and Exception object caches.
    exceptions: ExceptionObjects,
}

/// The tags and exception objects of one agent (JS API #object-caches, #js-exceptions).
#[derive(Default)]
struct ExceptionObjects {
    /// Tag addresses by integer handle; handles are never reused.
    tags: Vec<wasmi::Tag>,
    tag_ids: HashMap<wasmi::Tag, usize>,
    /// The handle of the agent's JavaScript exception tag, « externref » → « », allocated on
    /// first use ("get the JavaScript exception tag").
    js_tag: Option<usize>,
    /// The Tag object cache by tag handle. Its handles are internal edges without roots: a
    /// Tag object that JavaScript no longer references cannot be observed again, so a later
    /// lookup may create a new one.
    tag_cache: HashMap<usize, Value>,
    /// The Exception object cache by exception address. Each entry holds its exception once
    /// (`wasmi::ExnRef::hold`), and HostGc keeps the entry while Wasm can still reach the
    /// exception, so that throwing it out again yields the same object.
    exception_cache: HashMap<u32, Value>,
}

impl ExceptionObjects {
    fn register_tag(&mut self, tag: wasmi::Tag) -> usize {
        register(&mut self.tags, &mut self.tag_ids, tag)
    }

    /// "get the JavaScript exception tag": the agent's tag for JavaScript exceptions.
    fn js_tag(&mut self, context: impl wasmi::AsContextMut) -> usize {
        if let Some(id) = self.js_tag {
            return id;
        }
        let tag = wasmi::Tag::new(context, wasmi::TagType::new([wasmi::ValType::ExternRef]));
        let id = self.register_tag(tag);
        self.js_tag = Some(id);
        id
    }
}

impl WasmState {
    fn new() -> Self {
        let mut config = wasmi::Config::default();
        config.native_jit(!matches!(
            std::env::var("TRUST_WASM_NATIVE_JIT").as_deref(),
            Ok("0" | "false")
        ));
        let engine = wasmi::Engine::new(&config);
        let store = wasmi::Store::new(&engine, StoreData::default());
        Self {
            engine,
            store,
            modules: Vec::new(),
            instances: Vec::new(),
            funcs: Vec::new(),
            func_ids: HashMap::new(),
            function_cache: HashMap::new(),
            globals: Vec::new(),
            global_ids: HashMap::new(),
            memories: Vec::new(),
            tables: Vec::new(),
            table_ids: HashMap::new(),
            exceptions: ExceptionObjects::default(),
        }
    }

    fn register_func(&mut self, value: wasmi::Func) -> usize {
        register(&mut self.funcs, &mut self.func_ids, value)
    }

    fn register_global(&mut self, value: wasmi::Global) -> usize {
        register(&mut self.globals, &mut self.global_ids, value)
    }

    fn memory_id(&self, value: wasmi::Memory) -> Option<usize> {
        self.memories
            .iter()
            .position(|candidate| candidate.memory == value)
    }

    fn register_memory(&mut self, value: wasmi::Memory) -> Option<usize> {
        if let Some(index) = self.memory_id(value) {
            return Some(index);
        }
        let pages = value.size(&self.store);
        let storage = share_memory(value, &mut self.store)?;
        Some(self.push_memory(value, storage, pages))
    }

    fn push_memory(
        &mut self,
        value: wasmi::Memory,
        storage: ArrayBufferBytes,
        pages: u64,
    ) -> usize {
        let index = self.memories.len();
        self.memories.push(MemorySlot {
            memory: value,
            storage,
            buffer_pages: pages,
        });
        wasm_trace!(format!(
            "memory register id={index} pages={pages} bytes={}",
            pages.saturating_mul(65_536)
        ));
        index
    }

    fn register_table(&mut self, value: wasmi::Table) -> usize {
        register(&mut self.tables, &mut self.table_ids, value)
    }
}

/// Returns the agent-wide integer handle of a store object, assigning the next one on first
/// sight. Handles are never reused, so a handle in a JS internal slot always names one object.
fn register<T: Copy + Eq + std::hash::Hash>(
    items: &mut Vec<T>,
    ids: &mut HashMap<T, usize>,
    value: T,
) -> usize {
    *ids.entry(value).or_insert_with(|| {
        items.push(value);
        items.len() - 1
    })
}

/// Shares `memory`'s bytes as the Data Block of its `Memory.buffer` objects (JS API #memories:
/// the buffer's [[ArrayBufferData]] is "identified with" the memory). `None` only for a memory
/// over a static buffer, which this embedding never creates.
fn share_memory(
    memory: wasmi::Memory,
    context: impl wasmi::AsContextMut<Data = StoreData>,
) -> Option<ArrayBufferBytes> {
    // SAFETY: wasmi::Memory::share's contract.
    // - The store lives in this agent's `PageWasm` (an `Rc`, so it never leaves the agent
    //   thread), and Lumen keeps the returned `Rc` in ArrayBuffers of the same agent.
    // - Lumen touches the bytes only while JavaScript or a host operation runs: outside Wasm
    //   execution, or inside an imported-function callback, after which Wasmi reloads its
    //   view of memory 0 (other memories are resolved per access). This module never holds a
    //   slice of Wasm memory across a call into Lumen.
    let storage = unsafe { memory.share(context) };
    if storage.is_none() {
        wasm_trace!("memory share failed (static buffer)");
    }
    storage
}

fn page_wasm(ctx: &mut Ctx) -> Option<PageWasm> {
    ctx.host_mut::<HostState>().map(|state| state.wasm.clone())
}

pub(super) fn host_function_cache(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Undefined);
    };
    let Some(id) = arg_id(args, 0) else {
        return Ok(Value::Undefined);
    };
    let cache = |state: &mut WasmState| {
        if id >= state.funcs.len() {
            return Value::Undefined;
        }
        if let Some(wrapper) = state.function_cache.get(&id) {
            return wrapper.clone();
        }
        if let Some(wrapper) = args.get(1).filter(|value| value.is_callable()) {
            state.function_cache.insert(id, wrapper.clone());
            return wrapper.clone();
        }
        Value::Undefined
    };
    let result = match page.state.try_borrow_mut() {
        Ok(mut state) => state.as_mut().map(cache).unwrap_or(Value::Undefined),
        Err(_) => with_active_state(cache).unwrap_or(Value::Undefined),
    };
    Ok(result)
}

fn arg_id(args: &[Value], index: usize) -> Option<usize> {
    let number = args.get(index)?.as_num_opt()?;
    (number >= 0.0 && number.is_finite() && number.fract() == 0.0).then_some(number as usize)
}

fn arg_string(ctx: &mut Ctx, args: &[Value], index: usize) -> Result<String, Value> {
    ctx.coerce_string(args.get(index).unwrap_or(&Value::Undefined))
        .map(|value| value.to_string())
}

fn array_get(ctx: &mut Ctx, value: &Value, index: usize) -> Result<Value, Value> {
    ctx.member_get(value, &index.to_string())
}

fn envelope(ctx: &Ctx, code: impl Into<String>, value: Value) -> Value {
    ctx.make_array(vec![Value::from_string(code.into()), value])
}

fn wasm_ok(ctx: &Ctx, value: Value) -> Value {
    ctx.make_array(vec![Value::Num(0.0), value])
}

fn wasm_err(ctx: &Ctx, kind: &str, message: impl Into<String>) -> Value {
    envelope(ctx, kind, Value::from_string(message.into()))
}

fn type_error(ctx: &Ctx, message: impl Into<String>) -> Value {
    ctx.make_error("TypeError", message)
}

fn range_error(ctx: &Ctx, message: impl Into<String>) -> Value {
    ctx.make_error("RangeError", message)
}

fn wasm_kind(value: &wasmi::ExternType) -> &'static str {
    match value {
        wasmi::ExternType::Func(_) => "function",
        wasmi::ExternType::Table(_) => "table",
        wasmi::ExternType::Memory(_) => "memory",
        wasmi::ExternType::Global(_) => "global",
        // JS API #modules, ImportExportKind "tag".
        wasmi::ExternType::Tag(_) => "tag",
    }
}

fn value_type(name: &str) -> Option<wasmi::ValType> {
    match name {
        "i32" => Some(wasmi::ValType::I32),
        "i64" => Some(wasmi::ValType::I64),
        "f32" => Some(wasmi::ValType::F32),
        "f64" => Some(wasmi::ValType::F64),
        "v128" => Some(wasmi::ValType::V128),
        "externref" => Some(wasmi::ValType::ExternRef),
        "anyfunc" | "funcref" => Some(wasmi::ValType::FuncRef),
        _ => None,
    }
}

/// Returns `true` for the value types whose values never cross the JavaScript boundary.
///
/// WebAssembly JS API (local snapshot of the spec repository 37d6b059), "call an Exported
/// Function" step 4, "run a host function" step 2, `GetGlobalValue`, the `value` setter of
/// `Global` and `Table.get`/`Table.set`: signatures, globals and tables of type `v128` or
/// `exnref` throw a `TypeError` instead.
fn is_js_opaque(ty: wasmi::ValType) -> bool {
    matches!(ty, wasmi::ValType::V128 | wasmi::ValType::ExnRef)
}

fn to_i32(number: f64) -> i32 {
    if number == 0.0 || !number.is_finite() {
        return 0;
    }
    number.trunc().rem_euclid(4_294_967_296.0) as u32 as i32
}

fn js_to_numeric_wasm(
    ctx: &mut Ctx,
    value: &Value,
    ty: wasmi::ValType,
) -> Result<wasmi::Val, Value> {
    match ty {
        wasmi::ValType::I32 => Ok(wasmi::Val::I32(to_i32(ctx.coerce_number(value)?))),
        wasmi::ValType::I64 => Ok(wasmi::Val::I64(ctx.coerce_bigint_i64(value)?)),
        wasmi::ValType::F32 => Ok(wasmi::Val::F32(wasmi::F32::from(
            ctx.coerce_number(value)? as f32
        ))),
        wasmi::ValType::F64 => Ok(wasmi::Val::F64(wasmi::F64::from(ctx.coerce_number(value)?))),
        _ => Err(type_error(
            ctx,
            "WebAssembly: unsupported value type at the JavaScript boundary",
        )),
    }
}

fn numeric_wasm_to_js(ctx: &Ctx, value: &wasmi::Val) -> Result<Value, Value> {
    match value {
        wasmi::Val::I32(value) => Ok(Value::Num(f64::from(*value))),
        wasmi::Val::I64(value) => Ok(Value::bigint_from_i64(*value)),
        wasmi::Val::F32(value) => Ok(Value::Num(f64::from(value.to_float()))),
        wasmi::Val::F64(value) => Ok(Value::Num(value.to_float())),
        _ => Err(type_error(
            ctx,
            "WebAssembly: unsupported result type at the JavaScript boundary",
        )),
    }
}

/// Call one of the active Realm's private JS-side bridge operations. The WebAssembly bootstrap
/// installs them on the Realm's platform control object; none is reachable from author code.
fn call_bridge(ctx: &mut Ctx, name: &str, args: &[Value]) -> Result<Value, Value> {
    let control = super::platform_control(ctx)?;
    let bridge = ctx.member_get(&control, "wasm")?;
    let function = ctx.member_get(&bridge, name)?;
    ctx.invoke(function, Value::Undefined, args)
}

fn extern_intern(ctx: &mut Ctx, value: &Value) -> Result<usize, Value> {
    call_bridge(ctx, "externIntern", std::slice::from_ref(value)).map(|id| {
        id.as_num_opt()
            .filter(|number| *number >= 0.0)
            .unwrap_or(0.0) as usize
    })
}

fn extern_get(ctx: &mut Ctx, id: usize) -> Result<Value, Value> {
    call_bridge(ctx, "externGet", &[Value::Num(id as f64)])
}

fn make_exported_func(ctx: &mut Ctx, id: usize) -> Result<Value, Value> {
    call_bridge(ctx, "makeFunc", &[Value::Num(id as f64)])
}

#[derive(Clone, Copy)]
enum RefArg {
    Null(wasmi::ValType),
    Func(usize),
    Extern(usize),
}

fn prepare_ref(ctx: &mut Ctx, value: &Value, ty: wasmi::ValType) -> Result<RefArg, Value> {
    if matches!(value, Value::Null) {
        return Ok(RefArg::Null(ty));
    }
    match ty {
        wasmi::ValType::FuncRef => {
            // [[FunctionAddress]] is a private slot of an Exported Function, never a property
            // an author object could forge (WebAssembly JS API §5.6).
            let id = call_bridge(ctx, "funcId", std::slice::from_ref(value))?
                .as_num_opt()
                .filter(|number| *number >= 0.0)
                .map(|number| number as usize);
            id.map(RefArg::Func).ok_or_else(|| {
                type_error(
                    ctx,
                    "WebAssembly: a funcref must be an exported function or null",
                )
            })
        }
        wasmi::ValType::ExternRef => extern_intern(ctx, value).map(RefArg::Extern),
        _ => Err(type_error(ctx, "WebAssembly: unsupported reference type")),
    }
}

enum PreparedValue {
    Numeric(wasmi::Val),
    Reference(RefArg),
}

fn prepare_value(ctx: &mut Ctx, value: &Value, ty: wasmi::ValType) -> Result<PreparedValue, Value> {
    match ty {
        wasmi::ValType::FuncRef | wasmi::ValType::ExternRef => {
            prepare_ref(ctx, value, ty).map(PreparedValue::Reference)
        }
        _ => js_to_numeric_wasm(ctx, value, ty).map(PreparedValue::Numeric),
    }
}

fn build_ref<C: wasmi::AsContextMut<Data = StoreData>>(
    func: impl FnOnce(usize) -> Option<wasmi::Func>,
    context: &mut C,
    value: RefArg,
) -> Result<wasmi::Val, ()> {
    match value {
        RefArg::Null(wasmi::ValType::FuncRef) => Ok(wasmi::Val::FuncRef(wasmi::Nullable::Null)),
        RefArg::Null(wasmi::ValType::ExnRef) => Ok(wasmi::Val::ExnRef(wasmi::Nullable::Null)),
        RefArg::Null(_) => Ok(wasmi::Val::ExternRef(wasmi::Nullable::Null)),
        RefArg::Func(index) => func(index)
            .map(|function| wasmi::Val::FuncRef(wasmi::Nullable::Val(function)))
            .ok_or(()),
        RefArg::Extern(id) => {
            if let Some(reference) = context.as_context().data().externrefs.get(&id) {
                return Ok(wasmi::Val::ExternRef(wasmi::Nullable::Val(*reference)));
            }
            #[cfg(test)]
            EXTERNREF_ALLOCATIONS.with(|count| count.set(count.get() + 1));
            let reference = wasmi::ExternRef::new(&mut *context, id);
            context
                .as_context_mut()
                .data_mut()
                .externrefs
                .insert(id, reference);
            Ok(wasmi::Val::ExternRef(wasmi::Nullable::Val(reference)))
        }
    }
}

/// Wasmi 2 types table elements as references rather than general values.
fn table_ref(value: wasmi::Val) -> wasmi::Ref {
    match value {
        wasmi::Val::ExternRef(reference) => wasmi::Ref::Extern(reference),
        wasmi::Val::FuncRef(function) => wasmi::Ref::Func(function),
        wasmi::Val::ExnRef(reference) => wasmi::Ref::Exn(reference),
        // `prepare_ref`/`build_ref` only produce reference values.
        _ => wasmi::Ref::Func(wasmi::Nullable::Null),
    }
}

fn table_ref_type(element: wasmi::ValType) -> wasmi::RefType {
    match element {
        wasmi::ValType::ExternRef => wasmi::RefType::Extern,
        wasmi::ValType::ExnRef => wasmi::RefType::Exn,
        _ => wasmi::RefType::Func,
    }
}

/// [`build_ref`] against an idle agent store.
fn build_state_ref(state: &mut WasmState, value: RefArg) -> Result<wasmi::Val, ()> {
    let funcs = &state.funcs;
    build_ref(|index| funcs.get(index).copied(), &mut state.store, value)
}

/// [`build_ref`] while the store is lent to the active Caller. The function handle is resolved
/// before the Caller is borrowed, never by copying the agent's function table.
fn build_active_ref(value: RefArg) -> Option<wasmi::Val> {
    let func = match value {
        RefArg::Func(index) => Some(active_func(index)?),
        _ => None,
    };
    with_active_caller(|caller| build_ref(|_| func, caller, value)).and_then(Result::ok)
}

fn build_value(state: &mut WasmState, value: PreparedValue) -> Result<wasmi::Val, ()> {
    match value {
        PreparedValue::Numeric(value) => Ok(value),
        PreparedValue::Reference(value) => build_state_ref(state, value),
    }
}

enum OutputToken {
    Value(Value),
    Func(usize),
    Extern(usize),
}

fn extract_output<C: wasmi::AsContext<Data = StoreData>>(
    state: &mut WasmState,
    context: &C,
    value: &wasmi::Val,
) -> OutputToken {
    match value {
        wasmi::Val::FuncRef(wasmi::Nullable::Null)
        | wasmi::Val::ExternRef(wasmi::Nullable::Null) => OutputToken::Value(Value::Null),
        wasmi::Val::FuncRef(wasmi::Nullable::Val(function)) => {
            OutputToken::Func(state.register_func(*function))
        }
        wasmi::Val::ExternRef(wasmi::Nullable::Val(reference)) => OutputToken::Extern(
            reference
                .data(context)
                .downcast_ref::<usize>()
                .copied()
                .unwrap_or(0),
        ),
        value => OutputToken::Value(numeric_wasm_to_js_without_error(value)),
    }
}

fn numeric_wasm_to_js_without_error(value: &wasmi::Val) -> Value {
    match value {
        wasmi::Val::I32(value) => Value::Num(f64::from(*value)),
        wasmi::Val::I64(value) => Value::bigint_from_i64(*value),
        wasmi::Val::F32(value) => Value::Num(f64::from(value.to_float())),
        wasmi::Val::F64(value) => Value::Num(value.to_float()),
        _ => Value::Undefined,
    }
}

fn resolve_output(ctx: &mut Ctx, value: OutputToken) -> Result<Value, Value> {
    match value {
        OutputToken::Value(value) => Ok(value),
        OutputToken::Func(id) => make_exported_func(ctx, id),
        OutputToken::Extern(id) => extern_get(ctx, id),
    }
}

thread_local! {
    static ACTIVE_CTX: Cell<*mut Ctx> = const { Cell::new(std::ptr::null_mut()) };
    static ACTIVE_STATE: Cell<*mut WasmState> = const { Cell::new(std::ptr::null_mut()) };
    static ACTIVE_CALLER: Cell<*mut wasmi::Caller<'static, StoreData>> =
        const { Cell::new(std::ptr::null_mut()) };
    static PENDING_THROW: RefCell<Option<Value>> = const { RefCell::new(None) };
}

struct ContextGuard(*mut Ctx);

impl ContextGuard {
    fn set(ctx: &mut Ctx) -> Self {
        let previous = ACTIVE_CTX.with(|slot| slot.replace(ctx as *mut Ctx));
        PENDING_THROW.with(|slot| *slot.borrow_mut() = None);
        Self(previous)
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        ACTIVE_CTX.with(|slot| slot.set(self.0));
    }
}

struct StateGuard(*mut WasmState);

impl StateGuard {
    fn set(state: &mut WasmState) -> Self {
        Self(ACTIVE_STATE.with(|slot| slot.replace(state as *mut WasmState)))
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        ACTIVE_STATE.with(|slot| slot.set(self.0));
    }
}

struct CallerGuard(*mut wasmi::Caller<'static, StoreData>);

impl CallerGuard {
    fn set(caller: &mut wasmi::Caller<'_, StoreData>) -> Self {
        // SAFETY: the pointer remains thread-local and is restored before this synchronous host
        // callback returns. The erased lifetime is never exposed outside these helpers.
        let pointer = unsafe {
            std::mem::transmute::<
                *mut wasmi::Caller<'_, StoreData>,
                *mut wasmi::Caller<'static, StoreData>,
            >(std::ptr::from_mut(caller))
        };
        Self(ACTIVE_CALLER.with(|slot| slot.replace(pointer)))
    }
}

impl Drop for CallerGuard {
    fn drop(&mut self) {
        ACTIVE_CALLER.with(|slot| slot.set(self.0));
    }
}

fn with_active_state<R>(operation: impl FnOnce(&mut WasmState) -> R) -> Option<R> {
    ACTIVE_STATE.with(|slot| {
        let pointer = slot.get();
        (!pointer.is_null()).then(|| {
            // SAFETY: StateGuard installs the currently borrowed state and all calls are
            // synchronous on the page agent. Operations must not touch `state.store` while the
            // active Caller owns it.
            unsafe { operation(&mut *pointer) }
        })
    })
}

fn with_active_caller<R>(
    operation: impl FnOnce(&mut wasmi::Caller<'static, StoreData>) -> R,
) -> Option<R> {
    ACTIVE_CALLER.with(|slot| {
        let pointer = slot.get();
        (!pointer.is_null()).then(|| {
            // SAFETY: CallerGuard bounds this pointer to the synchronous import callback.
            unsafe { operation(&mut *pointer) }
        })
    })
}

fn active_func(index: usize) -> Option<wasmi::Func> {
    with_active_state(|state| state.funcs.get(index).copied()).flatten()
}

fn active_table(index: usize) -> Option<wasmi::Table> {
    with_active_state(|state| state.tables.get(index).copied()).flatten()
}

fn active_memory(index: usize) -> Option<wasmi::Memory> {
    with_active_state(|state| state.memories.get(index).map(|slot| slot.memory)).flatten()
}

/// [`WasmState::register_memory`] for the agent whose store is lent to the active Caller.
fn register_active_memory(memory: wasmi::Memory) -> Option<usize> {
    if let Some(id) = with_active_state(|state| state.memory_id(memory)).flatten() {
        return Some(id);
    }
    let (storage, pages) = with_active_caller(|caller| {
        let pages = memory.size(&*caller);
        share_memory(memory, &mut *caller).map(|storage| (storage, pages))
    })
    .flatten()?;
    with_active_state(|state| state.push_memory(memory, storage, pages))
}

fn active_instance(index: usize) -> Option<wasmi::Instance> {
    with_active_state(|state| state.instances.get(index).copied()).flatten()
}

fn module_for_page(page: &PageWasm, id: Option<usize>) -> Option<wasmi::Module> {
    match page.state.try_borrow() {
        Ok(state) => state
            .as_ref()
            .and_then(|state| id.and_then(|id| state.modules.get(id)))
            .cloned(),
        Err(_) => {
            id.and_then(|id| with_active_state(|state| state.modules.get(id).cloned()).flatten())
        }
    }
}

/// JS API #memories "refresh the Memory buffer": a fixed-length buffer of a memory that grew is
/// detached ("WebAssembly.Memory" key); the next `buffer` access creates its replacement over the
/// same, grown Data Block. Wasm `memory.grow` cannot run JavaScript, so performing this at each
/// transition back to JavaScript is indistinguishable from doing it right after the instruction.
fn refresh_buffers(
    ctx: &mut Ctx,
    memories: &mut [MemorySlot],
    context: impl wasmi::AsContext<Data = StoreData>,
    buffers: &mut [Option<Value>],
) {
    for (index, slot) in memories.iter_mut().enumerate() {
        let pages = slot.memory.size(&context);
        if pages == slot.buffer_pages {
            continue;
        }
        wasm_trace!(format!(
            "memory grow id={index} old_pages={} new_pages={pages}",
            slot.buffer_pages
        ));
        slot.buffer_pages = pages;
        if let Some(buffer) = buffers.get_mut(index).and_then(Option::take) {
            ctx.detach_array_buffer(&buffer);
        }
    }
}

/// [`refresh_buffers`] while the store is lent to an imported-function callback.
fn refresh_active_buffers(ctx: &mut Ctx) {
    let Some(page) = page_wasm(ctx) else {
        return;
    };
    let mut buffers = page.buffers.borrow_mut();
    let _ = with_active_caller(|caller| {
        with_active_state(|state| refresh_buffers(ctx, &mut state.memories, &*caller, &mut buffers))
    });
}

fn wasm_trap() -> wasmi::Error {
    wasmi::Error::new("WebAssembly host import raised a JavaScript exception")
}

/// Ends the Wasm execution with an uncatchable trap and rethrows `error` unchanged when it
/// returns to JavaScript.
fn pending_trap(error: Value) -> wasmi::Error {
    PENDING_THROW.with(|slot| *slot.borrow_mut() = Some(error));
    wasm_trap()
}

/// JS API "create a host function" step 3.10: the throw completion `thrown` of an imported
/// JavaScript function becomes a WebAssembly exception that Wasmi raises at the call site.
///
/// An `Exception` object throws its own exception address (step 3.10.2); any other value
/// becomes the `externref` payload of a new exception of the JavaScript exception tag (step
/// 3.10.3). A host interruption is no JavaScript exception (Lumen's `RuntimeInterrupt`): no
/// Wasm handler may catch it, so it unwinds as a trap and resumes in JavaScript.
fn thrown_to_wasm(ctx: &mut Ctx, thrown: Value) -> wasmi::Error {
    if ctx.pending_interrupt().is_some() {
        return pending_trap(thrown);
    }
    let address = match call_bridge(ctx, "exceptionAddress", std::slice::from_ref(&thrown)) {
        Ok(address) => address
            .as_num_opt()
            .filter(|address| (1.0..=f64::from(u32::MAX)).contains(address))
            .map(|address| address as u32),
        Err(error) => return pending_trap(error),
    };
    let exn = match address {
        Some(address) => with_active_caller(|caller| {
            let exn = wasmi::ExnRef::from_addr(&*caller, address)?;
            // The Exception object keeps its own hold; the error transfers this one.
            exn.hold(&*caller).then_some(exn)
        })
        .flatten(),
        None => {
            let payload = match prepare_ref(ctx, &thrown, wasmi::ValType::ExternRef) {
                Ok(payload) => payload,
                Err(error) => return pending_trap(error),
            };
            let Some(payload) = build_active_ref(payload) else {
                return pending_trap(thrown);
            };
            with_active_caller(|caller| {
                let tag = with_active_state(|state| {
                    let id = state.exceptions.js_tag(&mut *caller);
                    state.exceptions.tags[id]
                })?;
                wasmi::ExnRef::new(&mut *caller, &tag, &[payload]).ok()
            })
            .flatten()
        }
    };
    match exn {
        Some(exn) => wasmi::Error::exception(exn),
        None => pending_trap(thrown),
    }
}

fn import_results(
    ctx: &mut Ctx,
    returned: Value,
    types: &[wasmi::ValType],
    output: &mut [wasmi::Val],
) -> Result<(), Value> {
    match types {
        [] => Ok(()),
        [ty] => {
            let prepared = prepare_value(ctx, &returned, *ty)?;
            output[0] = match prepared {
                PreparedValue::Numeric(value) => value,
                PreparedValue::Reference(value) => build_active_ref(value)
                    .ok_or_else(|| type_error(ctx, "WebAssembly: invalid reference result"))?,
            };
            Ok(())
        }
        _ => {
            let values = call_bridge(ctx, "importResults", &[returned])?;
            let length = ctx
                .member_get(&values, "length")?
                .as_num_opt()
                .unwrap_or(0.0) as usize;
            if length != types.len() {
                return Err(type_error(
                    ctx,
                    "WebAssembly: multi-value import returned the wrong number of values",
                ));
            }
            for (index, ty) in types.iter().copied().enumerate() {
                let value = array_get(ctx, &values, index)?;
                let prepared = prepare_value(ctx, &value, ty)?;
                output[index] = match prepared {
                    PreparedValue::Numeric(value) => value,
                    PreparedValue::Reference(value) => build_active_ref(value)
                        .ok_or_else(|| type_error(ctx, "WebAssembly: invalid reference result"))?,
                };
            }
            Ok(())
        }
    }
}

fn make_import_func<C: wasmi::AsContextMut<Data = StoreData>>(
    context: &mut C,
    ty: wasmi::FuncType,
    token: u32,
    index: u32,
) -> wasmi::Func {
    let result_types = ty.results().to_vec();
    let opaque = ty
        .params()
        .iter()
        .chain(ty.results())
        .copied()
        .any(is_js_opaque);
    wasmi::Func::new(context, ty, move |mut caller, params, output| {
        let context_pointer = ACTIVE_CTX.with(Cell::get);
        if context_pointer.is_null() {
            return Err(wasmi::Error::new(
                "WebAssembly host call outside a JavaScript context",
            ));
        }
        // SAFETY: ContextGuard leaves the initiating `&mut Ctx` dormant while wasmi runs. This
        // callback is synchronous and executes on the same agent thread.
        let ctx = unsafe { &mut *context_pointer };
        let _caller_guard = CallerGuard::set(&mut caller);
        if opaque {
            // "run a host function" step 2 throws before the JavaScript function runs; like
            // every throw completion of a host function, the TypeError is catchable in Wasm.
            let error = type_error(
                ctx,
                "WebAssembly: cannot call a host function whose signature contains v128 or exnref",
            );
            return Err(thrown_to_wasm(ctx, error));
        }
        refresh_active_buffers(ctx);
        let mut arguments = Vec::with_capacity(params.len());
        for value in params {
            let token = match value {
                wasmi::Val::FuncRef(wasmi::Nullable::Val(function)) => {
                    let id = with_active_state(|state| state.register_func(*function)).unwrap_or(0);
                    make_exported_func(ctx, id)
                }
                wasmi::Val::ExternRef(wasmi::Nullable::Val(reference)) => {
                    let id = with_active_caller(|caller| {
                        reference.data(&*caller).downcast_ref::<usize>().copied()
                    })
                    .flatten()
                    .unwrap_or(0);
                    extern_get(ctx, id)
                }
                wasmi::Val::FuncRef(wasmi::Nullable::Null)
                | wasmi::Val::ExternRef(wasmi::Nullable::Null) => Ok(Value::Null),
                value => numeric_wasm_to_js(ctx, value),
            };
            match token {
                Ok(value) => arguments.push(value),
                Err(error) => return Err(thrown_to_wasm(ctx, error)),
            }
        }
        let result = call_bridge(
            ctx,
            "invokeImport",
            &[
                Value::Num(f64::from(token)),
                Value::Num(f64::from(index)),
                ctx.make_array(arguments),
            ],
        )
        .and_then(|returned| import_results(ctx, returned, &result_types, output));
        if let Err(error) = &result {
            // Inspect only engine-owned Error data, never [[Get]] or toString.
            // The diagnostic is bounded and cannot run author code or alter the throw (§7).
            wasm_trace!(format!(
                "import error token={token} index={index} params={params:?} error={:?}",
                ctx.error_diagnostic(error)
            ));
        }
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(thrown_to_wasm(ctx, error)),
        }
    })
}

fn error_kind(error: &wasmi::Error) -> &'static str {
    if error.as_trap_code().is_some() {
        "Runtime"
    } else {
        "Link"
    }
}

/// How a Wasm invocation (an exported call or a start function) ended without results.
enum Failure {
    /// A trap or another error: the JS API error class (see [`error_kind`]) and message.
    Error(&'static str, String),
    /// An uncaught WebAssembly exception.
    Thrown(Thrown),
}

impl From<(&'static str, String)> for Failure {
    fn from((kind, message): (&'static str, String)) -> Self {
        Self::Error(kind, message)
    }
}

/// An uncaught WebAssembly exception on its way to JavaScript ("call an Exported Function"
/// step 11).
enum Thrown {
    /// Step 11.4.1: an exception of the JavaScript exception tag throws its payload again.
    Value(OutputToken),
    /// Step 11.5.1: the Exception object cache already holds the exception's object.
    Object(Value),
    /// Step 11.5.1: a new Exception object for the exception `address` of the tag `tag`. The
    /// hold of the uncaught exception passes to the object's cache entry.
    New { address: u32, tag: usize },
}

/// Classifies the error that ended a Wasm invocation; an uncaught exception's store is
/// `context` and its tags are `objects`.
fn failure(
    error: wasmi::Error,
    objects: &mut ExceptionObjects,
    context: impl wasmi::AsContextMut<Data = StoreData>,
) -> Failure {
    match error
        .as_exception()
        .and_then(|exn| uncaught_exception(objects, context, exn))
    {
        Some(thrown) => Failure::Thrown(thrown),
        None => Failure::Error(error_kind(&error), error.to_string()),
    }
}

/// "call an Exported Function" steps 11.1–11.5 for the uncaught exception `exn`, whose hold
/// the caller owns. Returns `None` for an exception the store no longer knows.
fn uncaught_exception(
    objects: &mut ExceptionObjects,
    mut context: impl wasmi::AsContextMut<Data = StoreData>,
    exn: wasmi::ExnRef,
) -> Option<Thrown> {
    let tag = exn.tag(&context)?;
    let js_tag = objects.js_tag(&mut context);
    if objects.tags.get(js_tag) == Some(&tag) {
        // The payload is the externref of the thrown JavaScript value (ToJSValue).
        let token = match exn.field(&context, 0) {
            Some(wasmi::Val::ExternRef(wasmi::Nullable::Val(reference))) => OutputToken::Extern(
                reference
                    .data(&context)
                    .downcast_ref::<usize>()
                    .copied()
                    .unwrap_or(0),
            ),
            _ => OutputToken::Value(Value::Null),
        };
        exn.release(&context);
        return Some(Thrown::Value(token));
    }
    let address = exn.addr(&context)?;
    if let Some(object) = objects.exception_cache.get(&address) {
        exn.release(&context);
        return Some(Thrown::Object(object.clone()));
    }
    Some(Thrown::New {
        address,
        tag: objects.register_tag(tag),
    })
}

/// [`failure`] against the store of the idle agent `state`.
fn state_failure(state: &mut WasmState, error: wasmi::Error) -> Failure {
    failure(error, &mut state.exceptions, &mut state.store)
}

/// [`failure`] while the store is lent to the active Caller.
fn active_failure(error: wasmi::Error) -> Failure {
    let Some(exn) = error.as_exception() else {
        return Failure::Error(error_kind(&error), error.to_string());
    };
    let thrown = with_active_caller(|caller| {
        with_active_state(|state| uncaught_exception(&mut state.exceptions, &mut *caller, exn))
    })
    .flatten()
    .flatten();
    match thrown {
        Some(thrown) => Failure::Thrown(thrown),
        None => Failure::Error(error_kind(&error), error.to_string()),
    }
}

/// Turns an uncaught exception into the value JavaScript throws, creating and caching a new
/// Exception object ("create an Exception object") if needed.
fn thrown_to_js(ctx: &mut Ctx, page: &PageWasm, thrown: Thrown) -> Value {
    match thrown {
        Thrown::Value(token) => resolve_output(ctx, token).unwrap_or_else(|error| error),
        Thrown::Object(object) => object,
        Thrown::New { address, tag } => {
            match call_bridge(
                ctx,
                "makeException",
                &[Value::Num(f64::from(address)), Value::Num(tag as f64)],
            ) {
                Ok(object) => {
                    cache_exception(page, address, object.clone());
                    object
                }
                Err(error) => {
                    release_exception(page, address);
                    error
                }
            }
        }
    }
}

/// Enters `object` in the agent's Exception object cache for the exception `address`, whose
/// hold passes to the entry.
fn cache_exception(page: &PageWasm, address: u32, object: Value) {
    let insert = |state: &mut WasmState| state.exceptions.exception_cache.insert(address, object);
    let previous = match page.state.try_borrow_mut() {
        Ok(mut state) => state.as_mut().map(insert),
        Err(_) => with_active_state(insert),
    };
    if previous.flatten().is_some() {
        // "initialize an Exception object" asserts a fresh address; never leak a hold.
        release_exception(page, address);
    }
}

/// Releases one hold of the exception `address`.
fn release_exception(page: &PageWasm, address: u32) {
    match page.state.try_borrow_mut() {
        Ok(state) => {
            if let Some(state) = state.as_ref()
                && let Some(exn) = wasmi::ExnRef::from_addr(&state.store, address)
            {
                exn.release(&state.store);
            }
        }
        Err(_) => {
            let _ = with_active_caller(|caller| {
                if let Some(exn) = wasmi::ExnRef::from_addr(&*caller, address) {
                    exn.release(&*caller);
                }
            });
        }
    }
}

pub(super) fn host_validate(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(bytes) = args
        .first()
        .and_then(|value| ctx.buffer_source_bytes(value, true))
    else {
        return Ok(Value::Bool(false));
    };
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Bool(false));
    };
    let engine = match page.state.try_borrow_mut() {
        Ok(mut state) => state.get_or_insert_with(WasmState::new).engine.clone(),
        Err(_) => with_active_state(|state| state.engine.clone())
            .ok_or_else(|| type_error(ctx, "WebAssembly is unavailable"))?,
    };
    Ok(Value::Bool(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wasmi::Module::validate(&engine, &bytes).is_ok()
        }))
        .unwrap_or(false),
    ))
}

pub(super) fn host_compile(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(bytes) = args
        .first()
        .and_then(|value| ctx.buffer_source_bytes(value, true))
    else {
        return Ok(Value::from_string(
            "WebAssembly: expected a BufferSource".to_string(),
        ));
    };
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::from_string(
            "WebAssembly is unavailable on this page".to_string(),
        ));
    };
    let engine = match page.state.try_borrow_mut() {
        Ok(mut state) => state.get_or_insert_with(WasmState::new).engine.clone(),
        Err(_) => with_active_state(|state| state.engine.clone())
            .ok_or_else(|| type_error(ctx, "WebAssembly is unavailable"))?,
    };
    let module = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wasmi::Module::new(&engine, &bytes)
    })) {
        Ok(Ok(module)) => module,
        Ok(Err(error)) => return Ok(Value::from_string(error.to_string())),
        Err(_) => {
            return Ok(Value::from_string(
                "WebAssembly module uses an unsupported feature".to_string(),
            ));
        }
    };
    let import_count = module.imports().count();
    let export_count = module.exports().count();
    let id = match page.state.try_borrow_mut() {
        Ok(mut state) => {
            let state = state.get_or_insert_with(WasmState::new);
            let id = state.modules.len();
            state.modules.push(module);
            id
        }
        Err(_) => with_active_state(|state| {
            let id = state.modules.len();
            state.modules.push(module);
            id
        })
        .ok_or_else(|| type_error(ctx, "WebAssembly is unavailable"))?,
    };
    wasm_trace!(format!(
        "compile id={id} imports={import_count} exports={export_count} bytes={}",
        bytes.len()
    ));
    Ok(Value::Num(id as f64))
}

pub(super) fn host_module_imports(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(ctx.make_array(Vec::new()));
    };
    let mut output = Vec::new();
    if let Some(module) = module_for_page(&page, arg_id(args, 0)) {
        for import in module.imports() {
            output.push(Value::from_string(import.module().to_string()));
            output.push(Value::from_string(import.name().to_string()));
            output.push(Value::from_string(wasm_kind(import.ty()).to_string()));
        }
    }
    Ok(ctx.make_array(output))
}

pub(super) fn host_module_exports(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(ctx.make_array(Vec::new()));
    };
    let mut output = Vec::new();
    if let Some(module) = module_for_page(&page, arg_id(args, 0)) {
        for export in module.exports() {
            output.push(Value::from_string(export.name().to_string()));
            output.push(Value::from_string(wasm_kind(export.ty()).to_string()));
        }
    }
    Ok(ctx.make_array(output))
}

pub(super) fn host_module_custom_sections(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let name = arg_string(ctx, args, 1)?;
    let Some(page) = page_wasm(ctx) else {
        return Ok(ctx.make_array(Vec::new()));
    };
    let sections: Vec<Vec<u8>> = module_for_page(&page, arg_id(args, 0))
        .as_ref()
        .map(|module| {
            module
                .custom_sections()
                .filter(|section| section.name() == name)
                .map(|section| section.data().to_vec())
                .collect()
        })
        .unwrap_or_default();
    let mut output = Vec::with_capacity(sections.len());
    for section in sections {
        output.push(ctx.make_array_buffer(&section)?);
    }
    Ok(ctx.make_array(output))
}

enum ImportBinding {
    Func(u32),
    FuncRef(usize),
    GlobalRef(usize),
    GlobalValue(wasmi::Val),
    MemoryRef(usize),
    TableRef(usize),
    /// JS API "read the imports" step 4.11: the [[Address]] of a `WebAssembly.Tag`.
    TagRef(usize),
}

fn parse_imports(
    ctx: &mut Ctx,
    descriptor: Option<&Value>,
    types: &[wasmi::ExternType],
) -> Result<Vec<ImportBinding>, (&'static str, String)> {
    let descriptor = descriptor.cloned().unwrap_or(Value::Undefined);
    let mut output = Vec::with_capacity(types.len());
    for (index, ty) in types.iter().enumerate() {
        let entry = array_get(ctx, &descriptor, index)
            .map_err(|_| ("Link", format!("import {index}: missing binding")))?;
        let tag = array_get(ctx, &entry, 0)
            .and_then(|value| ctx.coerce_string(&value))
            .map(|value| value.to_string())
            .map_err(|_| ("Link", format!("import {index}: invalid binding")))?;
        let payload = array_get(ctx, &entry, 1).unwrap_or(Value::Undefined);
        match tag.as_str() {
            "f" => output.push(ImportBinding::Func(
                payload.as_num_opt().unwrap_or(0.0) as u32
            )),
            "fr" => output.push(ImportBinding::FuncRef(
                payload.as_num_opt().unwrap_or(0.0) as usize
            )),
            "g" => output.push(ImportBinding::GlobalRef(
                payload.as_num_opt().unwrap_or(0.0) as usize,
            )),
            "m" => output.push(ImportBinding::MemoryRef(
                payload.as_num_opt().unwrap_or(0.0) as usize,
            )),
            "t" => output.push(ImportBinding::TableRef(
                payload.as_num_opt().unwrap_or(0.0) as usize
            )),
            "tg" => output.push(ImportBinding::TagRef(
                payload.as_num_opt().unwrap_or(0.0) as usize
            )),
            "gv" => {
                let Some(global) = ty.global() else {
                    return Err((
                        "Link",
                        format!("import {index}: a value was given for a non-global import"),
                    ));
                };
                let value = js_to_numeric_wasm(ctx, &payload, global.content())
                    .map_err(|_| ("Link", format!("import {index}: incompatible global value")))?;
                output.push(ImportBinding::GlobalValue(value));
            }
            other => {
                return Err(("Link", format!("import {index}: unknown binding '{other}'")));
            }
        }
    }
    Ok(output)
}

fn instantiate_state(
    state: &mut WasmState,
    module: &wasmi::Module,
    token: u32,
    bindings: &[ImportBinding],
) -> Result<usize, Failure> {
    let mut linker = wasmi::Linker::new(&state.engine);
    for (index, import) in module.imports().enumerate() {
        let binding = bindings.get(index);
        let external = match (import.ty(), binding) {
            (wasmi::ExternType::Func(ty), Some(ImportBinding::Func(js_index))) => {
                wasmi::Extern::Func(make_import_func(
                    &mut state.store,
                    ty.clone(),
                    token,
                    *js_index,
                ))
            }
            (wasmi::ExternType::Func(_), Some(ImportBinding::FuncRef(id))) => state
                .funcs
                .get(*id)
                .copied()
                .map(wasmi::Extern::Func)
                .ok_or_else(|| {
                    (
                        "Link",
                        format!(
                            "import {}.{}: unknown function",
                            import.module(),
                            import.name()
                        ),
                    )
                })?,
            (wasmi::ExternType::Global(_), Some(ImportBinding::GlobalRef(id))) => state
                .globals
                .get(*id)
                .copied()
                .map(wasmi::Extern::Global)
                .ok_or_else(|| {
                    (
                        "Link",
                        format!(
                            "import {}.{}: unknown global",
                            import.module(),
                            import.name()
                        ),
                    )
                })?,
            (wasmi::ExternType::Global(_), Some(ImportBinding::GlobalValue(value))) => {
                let global =
                    wasmi::Global::new(&mut state.store, value.clone(), wasmi::Mutability::Const);
                state.register_global(global);
                wasmi::Extern::Global(global)
            }
            (wasmi::ExternType::Memory(_), Some(ImportBinding::MemoryRef(id))) => state
                .memories
                .get(*id)
                .map(|slot| wasmi::Extern::Memory(slot.memory))
                .ok_or_else(|| {
                    (
                        "Link",
                        format!(
                            "import {}.{}: unknown memory",
                            import.module(),
                            import.name()
                        ),
                    )
                })?,
            (wasmi::ExternType::Table(_), Some(ImportBinding::TableRef(id))) => state
                .tables
                .get(*id)
                .copied()
                .map(wasmi::Extern::Table)
                .ok_or_else(|| {
                    (
                        "Link",
                        format!(
                            "import {}.{}: unknown table",
                            import.module(),
                            import.name()
                        ),
                    )
                })?,
            (wasmi::ExternType::Tag(_), Some(ImportBinding::TagRef(id))) => state
                .exceptions
                .tags
                .get(*id)
                .copied()
                .map(wasmi::Extern::Tag)
                .ok_or_else(|| {
                    (
                        "Link",
                        format!("import {}.{}: unknown tag", import.module(), import.name()),
                    )
                })?,
            _ => {
                return Err(Failure::Error(
                    "Link",
                    format!(
                        "import {}.{}: JavaScript value has the wrong kind",
                        import.module(),
                        import.name()
                    ),
                ));
            }
        };
        linker
            .define(import.module(), import.name(), external)
            .map_err(|error| ("Link", error.to_string()))?;
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        linker.instantiate_and_start(&mut state.store, module)
    })) {
        Ok(Ok(instance)) => {
            let id = state.instances.len();
            state.instances.push(instance);
            Ok(id)
        }
        // An uncaught exception of the start function propagates like one of an exported call.
        Ok(Err(error)) => Err(state_failure(state, error)),
        Err(_) => Err(Failure::Error(
            "Link",
            "WebAssembly instantiation failed (unsupported feature)".to_string(),
        )),
    }
}

fn instantiate_active(
    module: &wasmi::Module,
    token: u32,
    bindings: &[ImportBinding],
) -> Result<usize, Failure> {
    let engine = with_active_state(|state| state.engine.clone()).ok_or((
        "Runtime",
        "WebAssembly: unavailable re-entrant store".to_string(),
    ))?;
    let mut linker = wasmi::Linker::new(&engine);
    for (index, import) in module.imports().enumerate() {
        let binding = bindings.get(index);
        let external = match (import.ty(), binding) {
            (wasmi::ExternType::Func(ty), Some(ImportBinding::Func(js_index))) => {
                with_active_caller(|caller| {
                    wasmi::Extern::Func(make_import_func(caller, ty.clone(), token, *js_index))
                })
                .ok_or((
                    "Runtime",
                    "WebAssembly: unavailable re-entrant store".to_string(),
                ))?
            }
            (wasmi::ExternType::Func(_), Some(ImportBinding::FuncRef(id))) => {
                active_func(*id).map(wasmi::Extern::Func).ok_or_else(|| {
                    (
                        "Link",
                        format!(
                            "import {}.{}: unknown function",
                            import.module(),
                            import.name()
                        ),
                    )
                })?
            }
            (wasmi::ExternType::Global(_), Some(ImportBinding::GlobalRef(id))) => {
                with_active_state(|state| state.globals.get(*id).copied())
                    .flatten()
                    .map(wasmi::Extern::Global)
                    .ok_or_else(|| {
                        (
                            "Link",
                            format!(
                                "import {}.{}: unknown global",
                                import.module(),
                                import.name()
                            ),
                        )
                    })?
            }
            (wasmi::ExternType::Global(_), Some(ImportBinding::GlobalValue(value))) => {
                let global = with_active_caller(|caller| {
                    wasmi::Global::new(caller, value.clone(), wasmi::Mutability::Const)
                })
                .ok_or((
                    "Runtime",
                    "WebAssembly: unavailable re-entrant store".to_string(),
                ))?;
                let _ = with_active_state(|state| state.register_global(global));
                wasmi::Extern::Global(global)
            }
            (wasmi::ExternType::Memory(_), Some(ImportBinding::MemoryRef(id))) => {
                active_memory(*id)
                    .map(wasmi::Extern::Memory)
                    .ok_or_else(|| {
                        (
                            "Link",
                            format!(
                                "import {}.{}: unknown memory",
                                import.module(),
                                import.name()
                            ),
                        )
                    })?
            }
            (wasmi::ExternType::Table(_), Some(ImportBinding::TableRef(id))) => {
                active_table(*id).map(wasmi::Extern::Table).ok_or_else(|| {
                    (
                        "Link",
                        format!(
                            "import {}.{}: unknown table",
                            import.module(),
                            import.name()
                        ),
                    )
                })?
            }
            (wasmi::ExternType::Tag(_), Some(ImportBinding::TagRef(id))) => {
                with_active_state(|state| state.exceptions.tags.get(*id).copied())
                    .flatten()
                    .map(wasmi::Extern::Tag)
                    .ok_or_else(|| {
                        (
                            "Link",
                            format!("import {}.{}: unknown tag", import.module(), import.name()),
                        )
                    })?
            }
            _ => {
                return Err(Failure::Error(
                    "Link",
                    format!(
                        "import {}.{}: JavaScript value has the wrong kind",
                        import.module(),
                        import.name()
                    ),
                ));
            }
        };
        linker
            .define(import.module(), import.name(), external)
            .map_err(|error| ("Link", error.to_string()))?;
    }
    let instantiated = with_active_caller(|caller| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            linker.instantiate_and_start(caller, module)
        }))
    })
    .ok_or((
        "Runtime",
        "WebAssembly: unavailable re-entrant store".to_string(),
    ))?;
    match instantiated {
        Ok(Ok(instance)) => with_active_state(|state| {
            let id = state.instances.len();
            state.instances.push(instance);
            id
        })
        .ok_or(Failure::Error(
            "Runtime",
            "WebAssembly: unavailable re-entrant state".to_string(),
        )),
        Ok(Err(error)) => Err(active_failure(error)),
        Err(_) => Err(Failure::Error(
            "Link",
            "WebAssembly instantiation failed (unsupported feature)".to_string(),
        )),
    }
}

pub(super) fn host_instantiate(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(wasm_err(ctx, "Link", "WebAssembly is unavailable"));
    };
    let module_id = arg_id(args, 0);
    let token = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let module = match page.state.try_borrow() {
        Ok(state) => state
            .as_ref()
            .and_then(|state| module_id.and_then(|id| state.modules.get(id)))
            .cloned(),
        Err(_) => module_id
            .and_then(|id| with_active_state(|state| state.modules.get(id).cloned()).flatten()),
    };
    let Some(module) = module else {
        return Ok(wasm_err(
            ctx,
            "Link",
            "WebAssembly.instantiate: unknown module",
        ));
    };
    let import_types: Vec<wasmi::ExternType> =
        module.imports().map(|import| import.ty().clone()).collect();
    wasm_trace!(format!(
        "instantiate module={} token={} imports={}",
        module_id.unwrap_or(usize::MAX),
        token,
        import_types.len()
    ));
    if trace_enabled() {
        let mut function_index = 0;
        for import in module.imports() {
            if matches!(import.ty(), wasmi::ExternType::Func(_)) {
                wasm_trace!(format!(
                    "import fn={} {}.{}",
                    function_index,
                    import.module(),
                    import.name()
                ));
                function_index += 1;
            }
        }
    }
    let bindings = match parse_imports(ctx, args.get(2), &import_types) {
        Ok(bindings) => bindings,
        Err((kind, message)) => return Ok(wasm_err(ctx, kind, message)),
    };
    let _context_guard = ContextGuard::set(ctx);
    let result = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let state = state_slot.get_or_insert_with(WasmState::new);
            let _state_guard = StateGuard::set(state);
            let result = instantiate_state(state, &module, token, &bindings);
            refresh_buffers(
                ctx,
                &mut state.memories,
                &state.store,
                &mut page.buffers.borrow_mut(),
            );
            result
        }
        Err(_) => {
            // WebAssembly Core §4.5 permits a host function to instantiate another
            // module. Reuse the active Caller so nested start functions and imports
            // execute in the same agent's associated store.
            let result = instantiate_active(&module, token, &bindings);
            refresh_active_buffers(ctx);
            result
        }
    };
    if let Some(error) = PENDING_THROW.with(|slot| slot.borrow_mut().take()) {
        return Err(error);
    }
    Ok(match result {
        Ok(id) => {
            wasm_trace!(format!("instantiate ok id={id}"));
            wasm_ok(ctx, Value::Num(id as f64))
        }
        Err(Failure::Error(kind, message)) => {
            wasm_trace!(format!("instantiate error kind={kind} message={message}"));
            wasm_err(ctx, kind, message)
        }
        // JS API "instantiate the core of a WebAssembly module" step 3: an exception of the
        // start function propagates as the object it is in JavaScript.
        Err(Failure::Thrown(thrown)) => return Err(thrown_to_js(ctx, &page, thrown)),
    })
}

pub(super) fn host_instance_exports(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(ctx.make_array(Vec::new()));
    };
    let instance_id = arg_id(args, 0);
    let module_id = arg_id(args, 1);
    let descriptors: Vec<(String, &'static str)> = match page.state.try_borrow() {
        Ok(state) => state
            .as_ref()
            .and_then(|state| module_id.and_then(|id| state.modules.get(id)))
            .map(|module| {
                module
                    .exports()
                    .map(|export| (export.name().to_string(), wasm_kind(export.ty())))
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => module_id
            .and_then(|id| {
                with_active_state(|state| {
                    state.modules.get(id).map(|module| {
                        module
                            .exports()
                            .map(|export| (export.name().to_string(), wasm_kind(export.ty())))
                            .collect()
                    })
                })
                .flatten()
            })
            .unwrap_or_default(),
    };
    wasm_trace!(format!(
        "exports instance={} module={} descriptors={:?}",
        instance_id.unwrap_or(usize::MAX),
        module_id.unwrap_or(usize::MAX),
        descriptors
            .iter()
            .map(|(name, kind)| format!("{name}:{kind}"))
            .collect::<Vec<_>>()
    ));
    let mut output = Vec::new();
    match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let Some(state) = state_slot.as_mut() else {
                return Ok(ctx.make_array(Vec::new()));
            };
            let Some(instance) = instance_id.and_then(|id| state.instances.get(id)).copied() else {
                return Ok(ctx.make_array(Vec::new()));
            };
            for (name, kind) in descriptors {
                let function_arity = if kind == "function" {
                    instance
                        .get_func(&state.store, &name)
                        .map(|function| function.ty(&state.store).params().len())
                        .unwrap_or(0)
                } else {
                    0
                };
                let id = match kind {
                    "function" => instance
                        .get_func(&state.store, &name)
                        .map(|value| state.register_func(value)),
                    "global" => instance
                        .get_global(&state.store, &name)
                        .map(|value| state.register_global(value)),
                    "memory" => instance
                        .get_memory(&state.store, &name)
                        .and_then(|value| state.register_memory(value)),
                    "table" => instance
                        .get_table(&state.store, &name)
                        .map(|value| state.register_table(value)),
                    "tag" => instance
                        .get_tag(&state.store, &name)
                        .map(|value| state.exceptions.register_tag(value)),
                    _ => None,
                };
                let Some(id) = id else { continue };
                let auxiliary = if kind == "function" {
                    function_arity.to_string()
                } else if kind == "table" {
                    state
                        .tables
                        .get(id)
                        .map(|table| match table.ty(&state.store).element() {
                            wasmi::RefType::Func => "anyfunc",
                            wasmi::RefType::Extern => "externref",
                            wasmi::RefType::Exn => "exnref",
                        })
                        .unwrap_or("")
                        .to_string()
                } else {
                    String::new()
                };
                output.push(Value::from_string(name));
                output.push(Value::from_string(kind.to_string()));
                output.push(Value::Num(id as f64));
                output.push(Value::from_string(auxiliary.to_string()));
            }
        }
        Err(_) => {
            let Some(instance) = instance_id.and_then(active_instance) else {
                return Ok(ctx.make_array(Vec::new()));
            };
            for (name, kind) in descriptors {
                let function_arity = if kind == "function" {
                    with_active_caller(|caller| {
                        instance
                            .get_func(&*caller, &name)
                            .map(|function| function.ty(&*caller).params().len())
                    })
                    .flatten()
                    .unwrap_or(0)
                } else {
                    0
                };
                let registered = if kind == "memory" {
                    with_active_caller(|caller| instance.get_memory(&*caller, &name))
                        .flatten()
                        .and_then(register_active_memory)
                } else {
                    with_active_caller(|caller| match kind {
                        "function" => instance.get_func(&*caller, &name).and_then(|value| {
                            with_active_state(|state| state.register_func(value))
                        }),
                        "global" => instance.get_global(&*caller, &name).and_then(|value| {
                            with_active_state(|state| state.register_global(value))
                        }),
                        "table" => instance.get_table(&*caller, &name).and_then(|value| {
                            with_active_state(|state| state.register_table(value))
                        }),
                        "tag" => instance.get_tag(&*caller, &name).and_then(|value| {
                            with_active_state(|state| state.exceptions.register_tag(value))
                        }),
                        _ => None,
                    })
                    .flatten()
                };
                let Some(id) = registered else { continue };
                let auxiliary = if kind == "function" {
                    function_arity.to_string()
                } else if kind == "table" {
                    with_active_caller(|caller| {
                        instance.get_table(&*caller, &name).map(|table| {
                            match table.ty(&*caller).element() {
                                wasmi::RefType::Func => "anyfunc",
                                wasmi::RefType::Extern => "externref",
                                wasmi::RefType::Exn => "exnref",
                            }
                        })
                    })
                    .flatten()
                    .unwrap_or("")
                    .to_string()
                } else {
                    String::new()
                };
                output.push(Value::from_string(name));
                output.push(Value::from_string(kind.to_string()));
                output.push(Value::Num(id as f64));
                output.push(Value::from_string(auxiliary.to_string()));
            }
        }
    }
    Ok(ctx.make_array(output))
}

pub(super) fn host_call_export(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Err(type_error(ctx, "WebAssembly is unavailable"));
    };
    let function_id = arg_id(args, 0);
    let signature = match page.state.try_borrow() {
        Ok(state) => state.as_ref().and_then(|state| {
            function_id
                .and_then(|id| state.funcs.get(id))
                .map(|function| {
                    let ty = function.ty(&state.store);
                    (ty.params().to_vec(), ty.results().to_vec())
                })
        }),
        Err(_) => function_id.and_then(active_func).and_then(|function| {
            with_active_caller(|caller| {
                let ty = function.ty(&*caller);
                (ty.params().to_vec(), ty.results().to_vec())
            })
        }),
    };
    let Some((parameters, results)) = signature else {
        return Err(type_error(
            ctx,
            "WebAssembly: call to an unknown exported function",
        ));
    };
    let trace_enabled = trace_enabled();
    let call_number = if trace_enabled {
        WASM_CALL_TRACE_COUNT.fetch_add(1, Ordering::Relaxed)
    } else {
        0
    };
    let trace_call = trace_enabled && (call_number < 64 || call_number % 1000 == 0);
    if trace_call {
        wasm_trace!(format!(
            "call #{} func={} params={:?} results={:?}",
            call_number + 1,
            function_id.unwrap_or(usize::MAX),
            parameters,
            results
        ));
    }
    if parameters.iter().chain(&results).copied().any(is_js_opaque) {
        return Err(type_error(
            ctx,
            "WebAssembly: cannot call a function whose signature contains v128 or exnref",
        ));
    }

    let arguments = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut prepared = Vec::with_capacity(parameters.len());
    for (index, ty) in parameters.iter().copied().enumerate() {
        let value = array_get(ctx, &arguments, index).unwrap_or(Value::Undefined);
        prepared.push(prepare_value(ctx, &value, ty)?);
    }

    let _context_guard = ContextGuard::set(ctx);
    let call: Result<Vec<OutputToken>, Failure> = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => (|| -> Result<Vec<OutputToken>, Failure> {
            let state = state_slot.get_or_insert_with(WasmState::new);
            let mut inputs = Vec::with_capacity(prepared.len());
            for value in prepared {
                inputs.push(build_value(state, value).map_err(|()| {
                    (
                        "Runtime",
                        "WebAssembly: invalid reference argument".to_string(),
                    )
                })?);
            }
            let function = function_id
                .and_then(|id| state.funcs.get(id))
                .copied()
                .ok_or(("Runtime", "WebAssembly: function went away".to_string()))?;
            let mut outputs: Vec<wasmi::Val> = results
                .iter()
                .copied()
                .map(wasmi::Val::default_for_ty)
                .collect();
            let _state_guard = StateGuard::set(state);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                function.call(&mut state.store, &inputs, &mut outputs)
            }));
            // JS API §§4.1, 5.6: publish the updated store before throwing. A trap
            // does not roll back writes or memory.grow (including buffer detachment).
            refresh_buffers(
                ctx,
                &mut state.memories,
                &state.store,
                &mut page.buffers.borrow_mut(),
            );
            let tokens = match result {
                Ok(Ok(())) => {
                    let store = &state.store as *const wasmi::Store<StoreData>;
                    outputs
                        .iter()
                        .map(|value| {
                            // SAFETY: the immutable store pointer remains valid for this
                            // extraction and does not overlap a mutable store operation.
                            extract_output(state, unsafe { &*store }, value)
                        })
                        .collect()
                }
                Ok(Err(error)) => return Err(state_failure(state, error)),
                Err(_) => return Err(("Runtime", "WebAssembly trap".to_string()).into()),
            };
            Ok(tokens)
        })(),
        Err(_) => (|| -> Result<Vec<OutputToken>, Failure> {
            let function = function_id.and_then(active_func).ok_or((
                "Runtime",
                "WebAssembly: unavailable re-entrant function".to_string(),
            ))?;
            let mut inputs = Vec::with_capacity(prepared.len());
            for value in prepared {
                inputs.push(match value {
                    PreparedValue::Numeric(value) => value,
                    PreparedValue::Reference(value) => build_active_ref(value).ok_or((
                        "Runtime",
                        "WebAssembly: invalid reference argument".to_string(),
                    ))?,
                });
            }
            let mut outputs: Vec<wasmi::Val> = results
                .iter()
                .copied()
                .map(wasmi::Val::default_for_ty)
                .collect();
            let invoked = with_active_caller(|caller| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    function.call(&mut *caller, &inputs, &mut outputs)
                }))
            })
            .ok_or((
                "Runtime",
                "WebAssembly: unavailable re-entrant store".to_string(),
            ))?;
            // The enclosing JS import may catch a nested trap and inspect Memory.buffer
            // immediately, before returning to the outer Wasm activation.
            refresh_active_buffers(ctx);
            match invoked {
                Ok(Ok(())) => {
                    let mut tokens = Vec::with_capacity(outputs.len());
                    for value in &outputs {
                        let token = with_active_caller(|caller| {
                            with_active_state(|state| extract_output(state, &*caller, value))
                        })
                        .flatten()
                        .ok_or((
                            "Runtime",
                            "WebAssembly: unavailable re-entrant result".to_string(),
                        ))?;
                        tokens.push(token);
                    }
                    Ok(tokens)
                }
                Ok(Err(error)) => Err(active_failure(error)),
                Err(_) => Err(("Runtime", "WebAssembly trap".to_string()).into()),
            }
        })(),
    };

    if let Some(error) = PENDING_THROW.with(|slot| slot.borrow_mut().take()) {
        return Err(error);
    }
    let tokens = match call {
        Ok(tokens) => tokens,
        Err(Failure::Error(kind, message)) => {
            if trace_call {
                wasm_trace!(format!(
                    "call #{} error kind={kind} message={message}",
                    call_number + 1
                ));
            }
            return Ok(wasm_err(ctx, kind, message));
        }
        Err(Failure::Thrown(thrown)) => {
            if trace_call {
                wasm_trace!(format!("call #{} uncaught exception", call_number + 1));
            }
            return Err(thrown_to_js(ctx, &page, thrown));
        }
    };
    let mut values = Vec::with_capacity(tokens.len());
    for token in tokens {
        values.push(resolve_output(ctx, token)?);
    }
    let value = match values.as_slice() {
        [] => Value::Undefined,
        [value] => value.clone(),
        _ => ctx.make_array(values),
    };
    Ok(wasm_ok(ctx, value))
}

pub(super) fn host_global_new(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let name = arg_string(ctx, args, 0)?;
    let mutable = matches!(args.get(1), Some(Value::Bool(true)));
    let Some(ty) = value_type(&name) else {
        return Err(type_error(ctx, "WebAssembly.Global: invalid value type"));
    };
    if matches!(ty, wasmi::ValType::V128) {
        return Err(type_error(
            ctx,
            "WebAssembly.Global: a v128 global cannot be constructed from JavaScript",
        ));
    }
    let initial = prepare_value(ctx, args.get(2).unwrap_or(&Value::Undefined), ty)?;
    let Some(page) = page_wasm(ctx) else {
        return Err(type_error(ctx, "WebAssembly is unavailable"));
    };
    let mutability = if mutable {
        wasmi::Mutability::Var
    } else {
        wasmi::Mutability::Const
    };
    let id = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let state = state_slot.get_or_insert_with(WasmState::new);
            let initial = build_value(state, initial)
                .map_err(|()| type_error(ctx, "WebAssembly.Global: invalid initial value"))?;
            let global = wasmi::Global::new(&mut state.store, initial, mutability);
            state.register_global(global)
        }
        Err(_) => {
            let initial = match initial {
                PreparedValue::Numeric(value) => value,
                PreparedValue::Reference(value) => build_active_ref(value)
                    .ok_or_else(|| type_error(ctx, "WebAssembly.Global: invalid initial value"))?,
            };
            let global =
                with_active_caller(|caller| wasmi::Global::new(caller, initial, mutability))
                    .ok_or_else(|| type_error(ctx, "WebAssembly is unavailable"))?;
            with_active_state(|state| state.register_global(global))
                .ok_or_else(|| type_error(ctx, "WebAssembly is unavailable"))?
        }
    };
    Ok(Value::Num(id as f64))
}

/// Returns the value type and mutability of the registered global `id`.
fn global_info(page: &PageWasm, id: Option<usize>) -> Option<(wasmi::ValType, wasmi::Mutability)> {
    match page.state.try_borrow() {
        Ok(state) => state.as_ref().and_then(|state| {
            id.and_then(|id| state.globals.get(id)).map(|global| {
                let ty = global.ty(&state.store);
                (ty.content(), ty.mutability())
            })
        }),
        Err(_) => id.and_then(|id| {
            let global = with_active_state(|state| state.globals.get(id).copied()).flatten()?;
            with_active_caller(|caller| {
                let ty = global.ty(&*caller);
                (ty.content(), ty.mutability())
            })
        }),
    }
}

pub(super) fn host_global_get(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Undefined);
    };
    let id = arg_id(args, 0);
    if global_info(&page, id).is_some_and(|(ty, _)| is_js_opaque(ty)) {
        // `GetGlobalValue` step 4.
        return Err(type_error(
            ctx,
            "WebAssembly.Global: a v128 or exnref value cannot be read from JavaScript",
        ));
    }
    let token = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => state_slot.as_mut().and_then(|state| {
            id.and_then(|id| state.globals.get(id))
                .copied()
                .map(|global| {
                    let value = global.get(&state.store);
                    let store = &state.store as *const wasmi::Store<StoreData>;
                    // SAFETY: see host_call_export's result extraction.
                    extract_output(state, unsafe { &*store }, &value)
                })
        }),
        Err(_) => id.and_then(|id| {
            let global = with_active_state(|state| state.globals.get(id).copied()).flatten()?;
            with_active_caller(|caller| {
                let value = global.get(&*caller);
                with_active_state(|state| extract_output(state, &*caller, &value))
            })
            .flatten()
        }),
    };
    match token {
        Some(token) => resolve_output(ctx, token),
        None => Ok(Value::Undefined),
    }
}

pub(super) fn host_global_set(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Undefined);
    };
    let id = arg_id(args, 0);
    let Some((ty, mutability)) = global_info(&page, id) else {
        return Ok(Value::Undefined);
    };
    if is_js_opaque(ty) {
        // The `value` setter, step 4.
        return Err(type_error(
            ctx,
            "WebAssembly.Global: a v128 or exnref value cannot be written from JavaScript",
        ));
    }
    if matches!(mutability, wasmi::Mutability::Const) {
        return Err(type_error(
            ctx,
            "WebAssembly.Global: cannot set an immutable global",
        ));
    }
    let prepared = prepare_value(ctx, args.get(1).unwrap_or(&Value::Undefined), ty)?;
    let result: Option<Result<(), String>> = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let Some(state) = state_slot.as_mut() else {
                return Ok(Value::Undefined);
            };
            let value = build_value(state, prepared)
                .map_err(|()| type_error(ctx, "WebAssembly.Global: invalid value"))?;
            id.and_then(|id| state.globals.get(id))
                .copied()
                .map(|global| {
                    global
                        .set(&mut state.store, value)
                        .map_err(|error| error.to_string())
                })
        }
        Err(_) => {
            let value = match prepared {
                PreparedValue::Numeric(value) => value,
                PreparedValue::Reference(value) => build_active_ref(value)
                    .ok_or_else(|| type_error(ctx, "WebAssembly.Global: invalid value"))?,
            };
            id.and_then(|id| {
                let global = with_active_state(|state| state.globals.get(id).copied()).flatten()?;
                with_active_caller(|caller| {
                    global.set(caller, value).map_err(|error| error.to_string())
                })
            })
        }
    };
    match result {
        Some(Ok(())) => Ok(Value::Undefined),
        Some(Err(error)) => Err(range_error(ctx, format!("WebAssembly.Global: {error}"))),
        None => Ok(Value::Undefined),
    }
}

pub(super) fn host_memory_new(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let initial = args.first().and_then(Value::as_num_opt).unwrap_or(-1.0);
    let maximum = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0);
    if !(0.0..=u32::MAX as f64).contains(&initial) || initial.fract() != 0.0 {
        return Err(range_error(
            ctx,
            "WebAssembly.Memory: initial is out of range",
        ));
    }
    let initial = initial as u64;
    let maximum = (maximum >= 0.0).then_some(maximum as u64);
    if maximum.is_some_and(|maximum| initial > maximum) {
        return Err(range_error(
            ctx,
            "WebAssembly.Memory: initial exceeds maximum",
        ));
    }
    let mut builder = wasmi::MemoryType::builder();
    builder.min(initial).max(maximum);
    let ty = builder
        .build()
        .map_err(|error| range_error(ctx, format!("WebAssembly.Memory: {error}")))?;
    let Some(page) = page_wasm(ctx) else {
        return Err(range_error(ctx, "WebAssembly is unavailable"));
    };
    let id = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let state = state_slot.get_or_insert_with(WasmState::new);
            let memory = wasmi::Memory::new(&mut state.store, ty)
                .map_err(|error| range_error(ctx, format!("WebAssembly.Memory: {error}")))?;
            state
                .register_memory(memory)
                .ok_or_else(|| range_error(ctx, "WebAssembly.Memory: unavailable memory"))?
        }
        Err(_) => {
            let memory = with_active_caller(|caller| wasmi::Memory::new(caller, ty))
                .ok_or_else(|| range_error(ctx, "WebAssembly is unavailable"))?
                .map_err(|error| range_error(ctx, format!("WebAssembly.Memory: {error}")))?;
            register_active_memory(memory)
                .ok_or_else(|| range_error(ctx, "WebAssembly.Memory: unavailable memory"))?
        }
    };
    wasm_trace!(format!(
        "memory new id={id} initial_pages={initial} max={maximum:?}"
    ));
    Ok(Value::Num(id as f64))
}

pub(super) fn host_memory_size(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Num(0.0));
    };
    let id = arg_id(args, 0);
    let pages = match page.state.try_borrow() {
        Ok(state) => state.as_ref().and_then(|state| {
            id.and_then(|id| state.memories.get(id))
                .map(|slot| slot.memory.size(&state.store))
        }),
        Err(_) => id
            .and_then(active_memory)
            .and_then(|memory| with_active_caller(|caller| memory.size(&*caller))),
    }
    .unwrap_or(0);
    Ok(Value::Num(pages as f64))
}

fn detach_buffer(ctx: &mut Ctx, buffers: &mut [Option<Value>], id: usize) {
    if let Some(buffer) = buffers.get_mut(id).and_then(Option::take) {
        ctx.detach_array_buffer(&buffer);
    }
}

pub(super) fn host_memory_grow(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let delta = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0);
    if !(0.0..=u32::MAX as f64).contains(&delta) || delta.fract() != 0.0 {
        return Err(range_error(ctx, "WebAssembly.Memory.grow: invalid delta"));
    }
    let Some(page) = page_wasm(ctx) else {
        return Err(range_error(ctx, "WebAssembly is unavailable"));
    };
    let id = arg_id(args, 0);
    let result = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let Some(state) = state_slot.as_mut() else {
                return Err(range_error(ctx, "WebAssembly.Memory.grow: unknown memory"));
            };
            id.and_then(|id| state.memories.get(id).map(|slot| slot.memory))
                .map(|memory| {
                    let result = memory.grow(&mut state.store, delta as u64);
                    if result.is_ok()
                        && let Some(id) = id
                        && let Some(slot) = state.memories.get_mut(id)
                    {
                        slot.buffer_pages = memory.size(&state.store);
                    }
                    result
                })
        }
        Err(_) => id.and_then(active_memory).and_then(|memory| {
            let result = with_active_caller(|caller| memory.grow(&mut *caller, delta as u64));
            if result.as_ref().is_some_and(Result::is_ok)
                && let Some(id) = id
            {
                let pages = with_active_caller(|caller| memory.size(&*caller)).unwrap_or(0);
                let _ = with_active_state(|state| {
                    if let Some(slot) = state.memories.get_mut(id) {
                        slot.buffer_pages = pages;
                    }
                });
            }
            result
        }),
    };
    match result {
        Some(Ok(old)) => {
            wasm_trace!(format!(
                "memory grow id={} delta={delta} old_pages={old}",
                id.unwrap_or(usize::MAX)
            ));
            // JS API "grow the memory buffer" refreshes the buffer even for a zero delta.
            if let Some(id) = id {
                detach_buffer(ctx, &mut page.buffers.borrow_mut(), id);
            }
            Ok(Value::Num(old as f64))
        }
        Some(Err(error)) => Err(range_error(
            ctx,
            format!("WebAssembly.Memory.grow: {error}"),
        )),
        None => Err(range_error(ctx, "WebAssembly.Memory.grow: unknown memory")),
    }
}

pub(super) fn host_memory_buffer(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Undefined);
    };
    let Some(id) = arg_id(args, 0) else {
        return Ok(Value::Undefined);
    };
    // The memory's current size and Data Block. A size different from the current buffer's
    // means a growth that no transition has published yet (the getter may run inside an
    // imported-function callback): refresh first, so the old buffer is detached.
    let current = |state: &mut WasmState, pages: u64| {
        let slot = state.memories.get_mut(id)?;
        let grew = slot.buffer_pages != pages;
        slot.buffer_pages = pages;
        Some((slot.storage.clone(), grew))
    };
    let current = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => state_slot.as_mut().and_then(|state| {
            let pages = state.memories.get(id)?.memory.size(&state.store);
            current(state, pages)
        }),
        Err(_) => active_memory(id).and_then(|memory| {
            let pages = with_active_caller(|caller| memory.size(&*caller))?;
            with_active_state(|state| current(state, pages)).flatten()
        }),
    };
    let Some((storage, grew)) = current else {
        return Ok(Value::Undefined);
    };
    if grew {
        detach_buffer(ctx, &mut page.buffers.borrow_mut(), id);
    }
    let existing = page
        .buffers
        .borrow()
        .get(id)
        .and_then(Option::as_ref)
        .cloned();
    if let Some(buffer) = existing {
        return Ok(buffer);
    }
    // JS API "create a fixed length memory buffer": a fresh ArrayBuffer whose
    // [[ArrayBufferData]] is the memory's Data Block and whose detach key is
    // "WebAssembly.Memory" (author code cannot transfer or detach it).
    wasm_trace!(format!("memory buffer id={id}"));
    let buffer = ctx.make_host_keyed_array_buffer_from_storage(storage)?;
    let mut buffers = page.buffers.borrow_mut();
    if buffers.len() <= id {
        buffers.resize(id + 1, None);
    }
    buffers[id] = Some(buffer.clone());
    Ok(buffer)
}

pub(super) fn host_table_new(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let element = arg_string(ctx, args, 0)?;
    let Some(element) = value_type(&element)
        .filter(|ty| matches!(ty, wasmi::ValType::FuncRef | wasmi::ValType::ExternRef))
    else {
        return Err(type_error(
            ctx,
            "WebAssembly.Table: element must be anyfunc or externref",
        ));
    };
    let initial = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0);
    let maximum = args.get(2).and_then(Value::as_num_opt).unwrap_or(-1.0);
    if !(0.0..=u32::MAX as f64).contains(&initial) || initial.fract() != 0.0 {
        return Err(range_error(
            ctx,
            "WebAssembly.Table: initial is out of range",
        ));
    }
    let initial = initial as u32;
    let maximum = (maximum >= 0.0).then_some(maximum as u32);
    if maximum.is_some_and(|maximum| initial > maximum) {
        return Err(range_error(
            ctx,
            "WebAssembly.Table: initial exceeds maximum",
        ));
    }
    let prepared = prepare_ref(ctx, args.get(3).unwrap_or(&Value::Undefined), element)?;
    let Some(page) = page_wasm(ctx) else {
        return Err(range_error(ctx, "WebAssembly is unavailable"));
    };
    let ty = wasmi::TableType::new(table_ref_type(element), initial, maximum);
    let id = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let state = state_slot.get_or_insert_with(WasmState::new);
            let initial_value = build_state_ref(state, prepared)
                .map_err(|()| type_error(ctx, "WebAssembly.Table: invalid initial value"))?;
            let table = wasmi::Table::new(&mut state.store, ty, table_ref(initial_value))
                .map_err(|error| range_error(ctx, format!("WebAssembly.Table: {error}")))?;
            state.register_table(table)
        }
        Err(_) => {
            let initial_value = build_active_ref(prepared)
                .ok_or_else(|| type_error(ctx, "WebAssembly.Table: invalid initial value"))?;
            let table = with_active_caller(|caller| {
                wasmi::Table::new(caller, ty, table_ref(initial_value))
            })
            .ok_or_else(|| range_error(ctx, "WebAssembly is unavailable"))?
            .map_err(|error| range_error(ctx, format!("WebAssembly.Table: {error}")))?;
            with_active_state(|state| state.register_table(table))
                .ok_or_else(|| range_error(ctx, "WebAssembly is unavailable"))?
        }
    };
    Ok(Value::Num(id as f64))
}

pub(super) fn host_table_length(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Num(0.0));
    };
    let id = arg_id(args, 0);
    let length = match page.state.try_borrow() {
        Ok(state) => state.as_ref().and_then(|state| {
            id.and_then(|id| state.tables.get(id))
                .map(|table| table.size(&state.store))
        }),
        Err(_) => id
            .and_then(active_table)
            .and_then(|table| with_active_caller(|caller| table.size(&*caller))),
    }
    .unwrap_or(0);
    Ok(Value::Num(length as f64))
}

pub(super) fn host_table_get(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Null);
    };
    let id = arg_id(args, 0);
    if table_info(&page, id) == Some(wasmi::ValType::ExnRef) {
        // `Table.get` step 4, before the index is converted.
        return Err(type_error(
            ctx,
            "WebAssembly.Table.get: exnref elements cannot be read from JavaScript",
        ));
    }
    let index = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0);
    if index < 0.0 || !index.is_finite() || index.fract() != 0.0 {
        return Err(range_error(
            ctx,
            "WebAssembly.Table.get: index is out of bounds",
        ));
    }
    let token = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let Some(state) = state_slot.as_mut() else {
                return Err(range_error(ctx, "WebAssembly.Table.get: unknown table"));
            };
            let value = id
                .and_then(|id| state.tables.get(id))
                .and_then(|table| table.get(&state.store, index as u64))
                .map(wasmi::Val::from);
            value.map(|value| {
                let store = &state.store as *const wasmi::Store<StoreData>;
                // SAFETY: immutable extraction only.
                extract_output(state, unsafe { &*store }, &value)
            })
        }
        Err(_) => id.and_then(active_table).and_then(|table| {
            with_active_caller(|caller| {
                table.get(&*caller, index as u64).and_then(|value| {
                    let value = wasmi::Val::from(value);
                    with_active_state(|state| extract_output(state, &*caller, &value))
                })
            })
            .flatten()
        }),
    };
    match token {
        Some(token) => resolve_output(ctx, token),
        None => Err(range_error(
            ctx,
            "WebAssembly.Table.get: index is out of bounds",
        )),
    }
}

fn table_info(page: &PageWasm, id: Option<usize>) -> Option<wasmi::ValType> {
    match page.state.try_borrow() {
        Ok(state) => state.as_ref().and_then(|state| {
            id.and_then(|id| state.tables.get(id))
                .map(|table| wasmi::ValType::from(table.ty(&state.store).element()))
        }),
        Err(_) => id.and_then(active_table).and_then(|table| {
            with_active_caller(|caller| wasmi::ValType::from(table.ty(&*caller).element()))
        }),
    }
}

pub(super) fn host_table_set(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Undefined);
    };
    let id = arg_id(args, 0);
    let Some(element) = table_info(&page, id) else {
        return Err(range_error(ctx, "WebAssembly.Table.set: unknown table"));
    };
    if element == wasmi::ValType::ExnRef {
        // `Table.set` step 4, before the index is converted.
        return Err(type_error(
            ctx,
            "WebAssembly.Table.set: exnref elements cannot be written from JavaScript",
        ));
    }
    let index = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0);
    if index < 0.0 || !index.is_finite() || index.fract() != 0.0 {
        return Err(range_error(
            ctx,
            "WebAssembly.Table.set: index is out of bounds",
        ));
    }
    let prepared = prepare_ref(ctx, args.get(2).unwrap_or(&Value::Undefined), element)?;
    let result: Option<Result<(), String>> = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let Some(state) = state_slot.as_mut() else {
                return Err(range_error(ctx, "WebAssembly.Table.set: unknown table"));
            };
            let value = build_state_ref(state, prepared)
                .map_err(|()| type_error(ctx, "WebAssembly.Table.set: invalid funcref"))?;
            id.and_then(|id| state.tables.get(id))
                .copied()
                .map(|table| {
                    table
                        .set(&mut state.store, index as u64, table_ref(value))
                        .map_err(|error| error.to_string())
                })
        }
        Err(_) => id.and_then(active_table).and_then(|table| {
            let value = build_active_ref(prepared);
            with_active_caller(|caller| match value {
                Some(value) => table
                    .set(caller, index as u64, table_ref(value))
                    .map_err(|error| error.to_string()),
                None => Err("invalid funcref".to_string()),
            })
        }),
    };
    match result {
        Some(Ok(())) => Ok(Value::Undefined),
        Some(Err(error)) => Err(range_error(ctx, format!("WebAssembly.Table.set: {error}"))),
        None => Err(range_error(ctx, "WebAssembly.Table.set: unknown table")),
    }
}

pub(super) fn host_table_grow(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Err(range_error(ctx, "WebAssembly is unavailable"));
    };
    let id = arg_id(args, 0);
    let delta = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0);
    if !(0.0..=u32::MAX as f64).contains(&delta) || delta.fract() != 0.0 {
        return Err(range_error(ctx, "WebAssembly.Table.grow: invalid delta"));
    }
    let Some(element) = table_info(&page, id) else {
        return Err(range_error(ctx, "WebAssembly.Table.grow: unknown table"));
    };
    let prepared = prepare_ref(ctx, args.get(2).unwrap_or(&Value::Undefined), element)?;
    let result: Option<Result<u64, String>> = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let Some(state) = state_slot.as_mut() else {
                return Err(range_error(ctx, "WebAssembly.Table.grow: unknown table"));
            };
            let value = build_state_ref(state, prepared)
                .map_err(|()| type_error(ctx, "WebAssembly.Table.grow: invalid funcref"))?;
            id.and_then(|id| state.tables.get(id))
                .copied()
                .map(|table| {
                    table
                        .grow(&mut state.store, delta as u64, table_ref(value))
                        .map_err(|error| error.to_string())
                })
        }
        Err(_) => id.and_then(active_table).and_then(|table| {
            let value = build_active_ref(prepared);
            with_active_caller(|caller| match value {
                Some(value) => table
                    .grow(caller, delta as u64, table_ref(value))
                    .map_err(|error| error.to_string()),
                None => Err("invalid funcref".to_string()),
            })
        }),
    };
    match result {
        Some(Ok(old)) => Ok(Value::Num(old as f64)),
        Some(Err(error)) => Err(range_error(ctx, format!("WebAssembly.Table.grow: {error}"))),
        None => Err(range_error(ctx, "WebAssembly.Table.grow: unknown table")),
    }
}

/// JS API #tags, #exceptions and the `JSTag` attribute: the store operations behind
/// `WebAssembly.Tag`, `WebAssembly.Exception` and `WebAssembly.JSTag`. The prelude performs
/// the Web IDL conversions and brand checks; `args[0]` names the operation.
pub(super) fn host_exception_binding(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let operation = arg_string(ctx, args, 0)?;
    let Some(page) = page_wasm(ctx) else {
        return Err(type_error(ctx, "WebAssembly is unavailable"));
    };
    match operation.as_str() {
        "tagNew" => tag_new(ctx, &page, args.get(1).unwrap_or(&Value::Undefined)),
        "jsTag" => js_tag(ctx, &page),
        "tagCache" => Ok(cached_object(
            &page,
            arg_id(args, 1),
            args.get(2),
            |objects| &mut objects.tag_cache,
        )),
        "exceptionNew" => exception_new(
            ctx,
            &page,
            arg_id(args, 1),
            args.get(2).unwrap_or(&Value::Undefined),
        ),
        "exceptionCache" => Ok(cached_object(
            &page,
            arg_id(args, 1),
            args.get(2),
            |objects| &mut objects.exception_cache,
        )),
        "exceptionArg" => exception_arg(ctx, &page, arg_id(args, 1), arg_id(args, 2)),
        _ => Err(type_error(ctx, "WebAssembly: unknown exception operation")),
    }
}

/// `new Tag(type)` steps 1–5 for the already converted `ValueType` strings `types`
/// (ToValueType, then `tag_alloc` of `parameters → « »`).
fn tag_new(ctx: &mut Ctx, page: &PageWasm, types: &Value) -> Result<Value, Value> {
    let length = ctx.member_get(types, "length")?.as_num_opt().unwrap_or(0.0) as usize;
    let mut params = Vec::with_capacity(length);
    for index in 0..length {
        let name = array_get(ctx, types, index)?;
        let name = ctx.coerce_string(&name)?.to_string();
        let Some(ty) = value_type(&name) else {
            return Err(type_error(ctx, "WebAssembly.Tag: invalid value type"));
        };
        params.push(ty);
    }
    let ty = wasmi::TagType::new(params);
    let id = match page.state.try_borrow_mut() {
        Ok(mut state) => {
            let state = state.get_or_insert_with(WasmState::new);
            let tag = wasmi::Tag::new(&mut state.store, ty);
            state.exceptions.register_tag(tag)
        }
        Err(_) => {
            let tag = with_active_caller(|caller| wasmi::Tag::new(caller, ty))
                .ok_or_else(|| type_error(ctx, "WebAssembly is unavailable"))?;
            with_active_state(|state| state.exceptions.register_tag(tag))
                .ok_or_else(|| type_error(ctx, "WebAssembly is unavailable"))?
        }
    };
    Ok(Value::Num(id as f64))
}

/// The getter of `WebAssembly.JSTag`, step 1: "get the JavaScript exception tag".
fn js_tag(ctx: &mut Ctx, page: &PageWasm) -> Result<Value, Value> {
    let id = match page.state.try_borrow_mut() {
        Ok(mut state) => {
            let state = state.get_or_insert_with(WasmState::new);
            Some(state.exceptions.js_tag(&mut state.store))
        }
        Err(_) => with_active_caller(|caller| {
            with_active_state(|state| state.exceptions.js_tag(&mut *caller))
        })
        .flatten(),
    };
    id.map(|id| Value::Num(id as f64))
        .ok_or_else(|| type_error(ctx, "WebAssembly is unavailable"))
}

/// Looks `key` up in one of the agent's object caches (JS API #object-caches), entering
/// `wrapper` if the cache has no object for it yet. Returns the cached object or undefined.
fn cached_object<K: Copy + Eq + std::hash::Hash + TryFrom<usize>>(
    page: &PageWasm,
    key: Option<usize>,
    wrapper: Option<&Value>,
    cache: impl Fn(&mut ExceptionObjects) -> &mut HashMap<K, Value>,
) -> Value {
    let Some(key) = key.and_then(|key| K::try_from(key).ok()) else {
        return Value::Undefined;
    };
    let lookup = |state: &mut WasmState| {
        let cache = cache(&mut state.exceptions);
        if let Some(object) = cache.get(&key) {
            return object.clone();
        }
        match wrapper.filter(|wrapper| matches!(wrapper, Value::Obj(_))) {
            Some(wrapper) => {
                cache.insert(key, wrapper.clone());
                wrapper.clone()
            }
            None => Value::Undefined,
        }
    };
    match page.state.try_borrow_mut() {
        Ok(mut state) => state.as_mut().map(lookup).unwrap_or(Value::Undefined),
        Err(_) => with_active_state(lookup).unwrap_or(Value::Undefined),
    }
}

/// `new Exception(exceptionTag, payload, options)` steps 1–8 for the tag handle `tag` and the
/// converted `sequence<any>` `payload`. Returns the address of the new exception, which is
/// held once for the Exception object cache entry that the prelude creates next.
fn exception_new(
    ctx: &mut Ctx,
    page: &PageWasm,
    tag: Option<usize>,
    payload: &Value,
) -> Result<Value, Value> {
    let unavailable = |ctx: &mut Ctx| type_error(ctx, "WebAssembly is unavailable");
    let Some(tag_id) = tag else {
        return Err(type_error(ctx, "WebAssembly.Exception: unknown tag"));
    };
    // Steps 1–4: the JavaScript exception tag, and the tag's parameter types.
    let found = match page.state.try_borrow_mut() {
        Ok(mut state) => {
            let state = state.get_or_insert_with(WasmState::new);
            exception_tag_types(&mut state.exceptions, &mut state.store, tag_id)
        }
        Err(_) => with_active_caller(|caller| {
            with_active_state(|state| {
                exception_tag_types(&mut state.exceptions, &mut *caller, tag_id)
            })
            .flatten()
        })
        .flatten(),
    };
    let Some((js_tag, tag, types)) = found else {
        return Err(unavailable(ctx));
    };
    if tag_id == js_tag {
        return Err(type_error(
            ctx,
            "WebAssembly.Exception: the JavaScript exception tag cannot be constructed",
        ));
    }
    // Step 5.
    let length = ctx
        .member_get(payload, "length")?
        .as_num_opt()
        .unwrap_or(0.0) as usize;
    if length != types.len() {
        return Err(type_error(
            ctx,
            "WebAssembly.Exception: the payload does not match the tag's parameters",
        ));
    }
    // Steps 6–7, in order: each type check precedes that value's conversion.
    let mut prepared = Vec::with_capacity(types.len());
    for (index, ty) in types.iter().copied().enumerate() {
        if is_js_opaque(ty) {
            return Err(type_error(
                ctx,
                "WebAssembly.Exception: a v128 or exnref value cannot be created from JavaScript",
            ));
        }
        let value = array_get(ctx, payload, index)?;
        prepared.push(prepare_value(ctx, &value, ty)?);
    }
    // Step 8: exn_alloc.
    let address = match page.state.try_borrow_mut() {
        Ok(mut state) => {
            let state = state.get_or_insert_with(WasmState::new);
            let mut values = Vec::with_capacity(prepared.len());
            for value in prepared {
                values.push(build_value(state, value).map_err(|()| unavailable(ctx))?);
            }
            wasmi::ExnRef::new(&mut state.store, &tag, &values)
                .ok()
                .and_then(|exn| exn.addr(&state.store))
        }
        Err(_) => {
            let mut values = Vec::with_capacity(prepared.len());
            for value in prepared {
                values.push(match value {
                    PreparedValue::Numeric(value) => value,
                    PreparedValue::Reference(value) => {
                        build_active_ref(value).ok_or_else(|| unavailable(ctx))?
                    }
                });
            }
            with_active_caller(|caller| {
                let exn = wasmi::ExnRef::new(&mut *caller, &tag, &values).ok()?;
                exn.addr(&*caller)
            })
            .flatten()
        }
    };
    address
        .map(|address| Value::Num(f64::from(address)))
        .ok_or_else(|| range_error(ctx, "WebAssembly.Exception: out of memory"))
}

/// Returns the JavaScript exception tag, the tag `tag_id` and its parameter types.
fn exception_tag_types(
    objects: &mut ExceptionObjects,
    mut context: impl wasmi::AsContextMut<Data = StoreData>,
    tag_id: usize,
) -> Option<(usize, wasmi::Tag, Vec<wasmi::ValType>)> {
    let js_tag = objects.js_tag(&mut context);
    let tag = *objects.tags.get(tag_id)?;
    Some((js_tag, tag, tag.ty(&context).params().to_vec()))
}

/// The outcome of [`read_exception_arg`].
enum ExceptionArg {
    /// `getArg` step 6: the index is not below the payload's size.
    OutOfBounds,
    /// `getArg` step 8: the field is a v128 or an exnref.
    Opaque,
    /// Step 9: the field, ToJSValue pending.
    Value(OutputToken),
}

/// Reads the field `index` of the exception `address` for `getArg`.
fn read_exception_arg<C: wasmi::AsContext<Data = StoreData>>(
    state: &mut WasmState,
    context: &C,
    address: u32,
    index: usize,
) -> Option<ExceptionArg> {
    let exn = wasmi::ExnRef::from_addr(context, address)?;
    let types = exn.tag(context)?.ty(context);
    let Some(ty) = types.params().get(index).copied() else {
        return Some(ExceptionArg::OutOfBounds);
    };
    if is_js_opaque(ty) {
        return Some(ExceptionArg::Opaque);
    }
    let field = exn.field(context, index)?;
    Some(ExceptionArg::Value(extract_output(state, context, &field)))
}

/// `getArg(exceptionTag, index)` steps 3–9 for the exception `address`, after the prelude
/// checked that `exceptionTag` is the exception's [[Type]] (step 2).
fn exception_arg(
    ctx: &mut Ctx,
    page: &PageWasm,
    address: Option<usize>,
    index: Option<usize>,
) -> Result<Value, Value> {
    let (Some(address), Some(index)) = (address.and_then(|a| u32::try_from(a).ok()), index) else {
        return Err(type_error(
            ctx,
            "WebAssembly.Exception.getArg: invalid arguments",
        ));
    };
    let arg = match page.state.try_borrow_mut() {
        Ok(mut state) => state.as_mut().and_then(|state| {
            let store = &state.store as *const wasmi::Store<StoreData>;
            // SAFETY: see host_call_export's result extraction.
            read_exception_arg(state, unsafe { &*store }, address, index)
        }),
        Err(_) => with_active_caller(|caller| {
            with_active_state(|state| read_exception_arg(state, &*caller, address, index)).flatten()
        })
        .flatten(),
    };
    match arg {
        Some(ExceptionArg::Value(token)) => resolve_output(ctx, token),
        Some(ExceptionArg::OutOfBounds) => Err(range_error(
            ctx,
            "WebAssembly.Exception.getArg: index out of bounds",
        )),
        Some(ExceptionArg::Opaque) => Err(type_error(
            ctx,
            "WebAssembly.Exception.getArg: a v128 or exnref value cannot be read from JavaScript",
        )),
        None => Err(type_error(
            ctx,
            "WebAssembly.Exception.getArg: unknown exception",
        )),
    }
}

#[cfg(test)]
mod tests {
    /// Wasmi 2.0's optimized interpreter only keeps a constant native stack while every
    /// handler reaches the next one with a sibling call (see the Wasmi profile overrides in
    /// Cargo.toml). A handler that loses it grows the stack once per executed operator, so
    /// this loop over common operator families (including growth, legacy exception unwinding,
    /// `try_table` catch clauses with `throw_ref`, host imports, and exceptions that a host
    /// function throws with an imported tag) would overflow the small thread stack and abort
    /// the test binary under `cargo test --release`. Debug builds use Wasmi's portable loop
    /// dispatch.
    #[test]
    fn wasm_interpreter_dispatch_keeps_a_constant_native_stack() {
        const MODULE: &str = r#"
            (module
              (import "host" "add" (func $host_add (param i64) (result i64)))
              (import "host" "throw" (func $host_throw (param i32)))
              (import "host" "tag" (tag $host_tag (param i32)))
              (memory 1)
              (global $g (mut i64) (i64.const 0))
              (type $t (func (param i64) (result i64)))
              (table 2 funcref)
              (elem (i32.const 0) $inc $dec)
              (func $inc (type $t) (i64.add (local.get 0) (i64.const 1)))
              (func $dec (type $t) (i64.sub (local.get 0) (i64.const 1)))
              (tag $tag (param i32))
              (func $throw (param i32) (throw $tag (local.get 0)))
              (func (export "run") (param $n i32) (result i64)
                (local $acc i64) (local $f f64) (local $v v128)
                (loop $again
                  (i32.store8 offset=7 (i32.and (local.get $n) (i32.const 255)) (local.get $n))
                  (i64.store offset=16 (i32.const 8) (local.get $acc))
                  (local.set $acc (i64.add (local.get $acc)
                    (i64.load8_u offset=7 (i32.and (local.get $n) (i32.const 255)))))
                  (local.set $acc (i64.xor (local.get $acc) (i64.load offset=16 (i32.const 8))))
                  (memory.copy (i32.const 64) (i32.const 0) (i32.const 16))
                  (local.set $f (f64.add (f64.mul (local.get $f) (f64.const 0.5))
                    (f64.convert_i32_u (local.get $n))))
                  (local.set $acc (i64.add (local.get $acc)
                    (i64.trunc_f64_u (f64.sqrt (local.get $f)))))
                  (local.set $v (i32x4.add (local.get $v) (i32x4.splat (local.get $n))))
                  (v128.store offset=32 (i32.const 0) (local.get $v))
                  (local.set $acc (i64.add (local.get $acc) (i64.extend_i32_u
                    (i32x4.extract_lane 2 (v128.load offset=32 (i32.const 0))))))
                  (local.set $acc (i64.add (local.get $acc)
                    (i64.extend_i32_u (i8x16.bitmask (local.get $v)))))
                  (global.set $g (i64.add (global.get $g) (local.get $acc)))
                  (local.set $acc (call $inc (local.get $acc)))
                  (local.set $acc (call_indirect (type $t) (local.get $acc)
                    (i32.and (local.get $n) (i32.const 1))))
                  (block $b2
                    (block $b1
                      (block $b0
                        (br_table $b0 $b1 $b2 (i32.rem_u (local.get $n) (i32.const 3))))
                      (local.set $acc (i64.rotl (local.get $acc) (i64.const 3)))
                      (br $b2))
                    (local.set $acc (select (local.get $acc) (i64.const 1)
                      (i32.eqz (local.get $n)))))
                  (local.set $acc (i64.add (local.get $acc)
                    (i64.extend_i32_u (ref.is_null (table.get (i32.const 1))))))
                  (drop (memory.grow (i32.const 0)))
                  (drop (table.grow (ref.null func) (i32.const 0)))
                  try (result i32)
                    try (result i32)
                      (call $throw (local.get $n))
                      i32.const 0
                    catch $tag
                      rethrow 0
                    end
                  catch $tag
                  end
                  i64.extend_i32_u
                  (local.set $acc (i64.add (local.get $acc)))
                  (block $h (result i32)
                    (try_table (result i32) (catch $tag $h)
                      (throw_ref (block $r (result exnref)
                        (try_table (catch_all_ref $r) (call $throw (local.get $n)))
                        (unreachable)))))
                  i64.extend_i32_u
                  (local.set $acc (i64.add (local.get $acc)))
                  (block $all (try_table (catch_all $all) (call $throw (local.get $n))))
                  (block $ref (result i32 exnref)
                    (try_table (catch_ref $tag $ref) (call $throw (local.get $n)))
                    (unreachable))
                  drop
                  i64.extend_i32_u
                  (local.set $acc (i64.add (local.get $acc)))
                  (local.set $acc (call $host_add (local.get $acc)))
                  (block $host (result i32)
                    (try_table (catch $host_tag $host) (call $host_throw (local.get $n)))
                    (i32.const 0))
                  i64.extend_i32_u
                  (local.set $acc (i64.add (local.get $acc)))
                  (br_if $again (local.tee $n (i32.sub (local.get $n) (i32.const 1)))))
                (i64.add (local.get $acc) (global.get $g))))
        "#;
        let run = |native_jit: bool| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut config = wasmi::Config::default();
                    config.native_jit(native_jit);
                    let engine = wasmi::Engine::new(&config);
                    let module = wasmi::Module::new(&engine, wat::parse_str(MODULE).unwrap())
                        .expect("valid module");
                    let mut store = wasmi::Store::new(&engine, ());
                    let tag =
                        wasmi::Tag::new(&mut store, wasmi::TagType::new([wasmi::ValType::I32]));
                    let mut linker = wasmi::Linker::new(&engine);
                    linker.define("host", "tag", tag).expect("define tag");
                    linker
                        .func_wrap("host", "add", |value: i64| value.wrapping_add(3))
                        .expect("define add");
                    linker
                        .func_wrap(
                            "host",
                            "throw",
                            move |mut caller: wasmi::Caller<'_, ()>,
                                  value: i32|
                                  -> Result<(), wasmi::Error> {
                                let fields = [wasmi::Val::I32(value)];
                                let exn = wasmi::ExnRef::new(&mut caller, &tag, &fields)?;
                                Err(wasmi::Error::exception(exn))
                            },
                        )
                        .expect("define throw");
                    let instance = linker
                        .instantiate_and_start(&mut store, &module)
                        .expect("instantiable module");
                    instance
                        .get_typed_func::<i32, i64>(&store, "run")
                        .expect("exported run")
                        .call(&mut store, 50_000)
                        .expect("completes without a trap")
                })
                .expect("spawn interpreter thread")
                .join()
                .expect("interpreter thread")
        };
        let interpreted = run(false);
        assert_eq!(
            run(true),
            interpreted,
            "native regions agree with the interpreter"
        );
    }
}
