//! WebAssembly JavaScript host boundary for the Lumen backend.
//!
//! The observable classes and promise APIs live in the shared platform prelude. This module owns
//! one wasmi store per JavaScript agent and translates the prelude's integer handles according to
//! the WebAssembly JavaScript Interface. Memory uses a keyed ArrayBuffer mirror: synchronization
//! happens at every JS/wasm transition, including imported-function callbacks, and growth detaches
//! the previous object before a replacement is exposed. This avoids aliasing wasmi's reallocating
//! storage through an unsafe raw ArrayBuffer while preserving the specified observable behavior.

use super::HostState;
use lumen::embed::{Ctx, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone)]
pub(super) struct PageWasm {
    state: Rc<RefCell<Option<WasmState>>>,
    buffers: Rc<RefCell<Vec<Option<Value>>>>,
}

impl PageWasm {
    pub(super) fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(None)),
            buffers: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

struct MemorySlot {
    memory: wasmi::Memory,
    buffer_pages: u64,
    /// Last JS-side ArrayBuffer generation committed to Wasmi.
    js_version: u64,
    /// Last Wasmi-side memory generation exposed to the JS ArrayBuffer mirror.
    wasm_version: u64,
}

struct WasmState {
    engine: wasmi::Engine,
    store: wasmi::Store<()>,
    modules: Vec<wasmi::Module>,
    instances: Vec<wasmi::Instance>,
    funcs: Vec<wasmi::Func>,
    globals: Vec<wasmi::Global>,
    memories: Vec<MemorySlot>,
    tables: Vec<wasmi::Table>,
}

impl WasmState {
    fn new() -> Self {
        let engine = wasmi::Engine::default();
        let store = wasmi::Store::new(&engine, ());
        Self {
            engine,
            store,
            modules: Vec::new(),
            instances: Vec::new(),
            funcs: Vec::new(),
            globals: Vec::new(),
            memories: Vec::new(),
            tables: Vec::new(),
        }
    }

    fn register_func(&mut self, value: wasmi::Func) -> usize {
        if let Some(index) = self.funcs.iter().position(|candidate| *candidate == value) {
            return index;
        }
        let index = self.funcs.len();
        self.funcs.push(value);
        index
    }

    fn register_global(&mut self, value: wasmi::Global) -> usize {
        if let Some(index) = self
            .globals
            .iter()
            .position(|candidate| *candidate == value)
        {
            return index;
        }
        let index = self.globals.len();
        self.globals.push(value);
        index
    }

    fn register_memory(&mut self, value: wasmi::Memory) -> usize {
        let pages = value.size(&self.store);
        self.register_memory_with_pages(value, pages)
    }

    fn register_memory_with_pages(&mut self, value: wasmi::Memory, pages: u64) -> usize {
        if let Some(index) = self
            .memories
            .iter()
            .position(|candidate| candidate.memory == value)
        {
            return index;
        }
        let index = self.memories.len();
        self.memories.push(MemorySlot {
            memory: value,
            buffer_pages: pages,
            js_version: 0,
            wasm_version: value.data_version(&self.store),
        });
        index
    }

    fn register_table(&mut self, value: wasmi::Table) -> usize {
        if let Some(index) = self.tables.iter().position(|candidate| *candidate == value) {
            return index;
        }
        let index = self.tables.len();
        self.tables.push(value);
        index
    }
}

fn page_wasm(ctx: &mut Ctx) -> Option<PageWasm> {
    ctx.host_mut::<HostState>().map(|state| state.wasm.clone())
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

fn call_global(ctx: &mut Ctx, name: &str, args: &[Value]) -> Result<Value, Value> {
    let global = ctx.global_this();
    let function = ctx.member_get(&global, name)?;
    ctx.invoke(function, Value::Undefined, args)
}

fn extern_intern(ctx: &mut Ctx, value: &Value) -> Result<usize, Value> {
    call_global(ctx, "__wasm_extern_intern", std::slice::from_ref(value)).map(|id| {
        id.as_num_opt()
            .filter(|number| *number >= 0.0)
            .unwrap_or(0.0) as usize
    })
}

fn extern_get(ctx: &mut Ctx, id: usize) -> Result<Value, Value> {
    call_global(ctx, "__wasm_extern_get", &[Value::Num(id as f64)])
}

fn make_exported_func(ctx: &mut Ctx, id: usize) -> Result<Value, Value> {
    call_global(ctx, "__wasm_make_func", &[Value::Num(id as f64)])
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
            let id = ctx
                .member_get(value, "__wasmFunc")?
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

fn build_ref<C: wasmi::AsContextMut<Data = ()>>(
    funcs: &[wasmi::Func],
    context: &mut C,
    value: RefArg,
) -> Result<wasmi::Val, ()> {
    match value {
        RefArg::Null(wasmi::ValType::FuncRef) => Ok(wasmi::Val::FuncRef(wasmi::Ref::Null)),
        RefArg::Null(_) => Ok(wasmi::Val::ExternRef(wasmi::Ref::Null)),
        RefArg::Func(index) => funcs
            .get(index)
            .copied()
            .map(|function| wasmi::Val::FuncRef(wasmi::Ref::Val(function)))
            .ok_or(()),
        RefArg::Extern(id) => Ok(wasmi::Val::ExternRef(wasmi::Ref::Val(
            wasmi::ExternRef::new(context, id),
        ))),
    }
}

fn build_value(state: &mut WasmState, value: PreparedValue) -> Result<wasmi::Val, ()> {
    match value {
        PreparedValue::Numeric(value) => Ok(value),
        PreparedValue::Reference(value) => build_ref(&state.funcs, &mut state.store, value),
    }
}

enum OutputToken {
    Value(Value),
    Func(usize),
    Extern(usize),
}

fn extract_output<C: wasmi::AsContext<Data = ()>>(
    state: &mut WasmState,
    context: &C,
    value: &wasmi::Val,
) -> OutputToken {
    match value {
        wasmi::Val::FuncRef(wasmi::Ref::Null) | wasmi::Val::ExternRef(wasmi::Ref::Null) => {
            OutputToken::Value(Value::Null)
        }
        wasmi::Val::FuncRef(wasmi::Ref::Val(function)) => {
            OutputToken::Func(state.register_func(*function))
        }
        wasmi::Val::ExternRef(wasmi::Ref::Val(reference)) => OutputToken::Extern(
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
    static ACTIVE_CALLER: Cell<*mut wasmi::Caller<'static, ()>> =
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

struct CallerGuard(*mut wasmi::Caller<'static, ()>);

impl CallerGuard {
    fn set(caller: &mut wasmi::Caller<'_, ()>) -> Self {
        // SAFETY: the pointer remains thread-local and is restored before this synchronous host
        // callback returns. The erased lifetime is never exposed outside these helpers.
        let pointer = unsafe {
            std::mem::transmute::<*mut wasmi::Caller<'_, ()>, *mut wasmi::Caller<'static, ()>>(
                std::ptr::from_mut(caller),
            )
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
    operation: impl FnOnce(&mut wasmi::Caller<'static, ()>) -> R,
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

fn sync_store_from_buffers(ctx: &mut Ctx, state: &mut WasmState, buffers: &[Option<Value>]) {
    for (index, slot) in state.memories.iter_mut().enumerate() {
        let Some(buffer) = buffers.get(index).and_then(Option::as_ref) else {
            continue;
        };
        let version = ctx.array_buffer_version(buffer).unwrap_or(0);
        if version == slot.js_version {
            continue;
        }
        let dirty_ranges = ctx
            .take_array_buffer_dirty_ranges(buffer)
            .unwrap_or_default();
        let data = slot.memory.data_mut(&mut state.store);
        let mirrored = if dirty_ranges.is_empty() {
            let Some(bytes) = ctx.buffer_source_bytes(buffer, false) else {
                continue;
            };
            if data.len() != bytes.len() {
                false
            } else {
                data.copy_from_slice(&bytes);
                true
            }
        } else {
            dirty_ranges.iter().all(|range| {
                let Some(destination) = data.get_mut(range.clone()) else {
                    return false;
                };
                let Some(length) = range.end.checked_sub(range.start) else {
                    return false;
                };
                if !ctx.array_buffer_copy_range(buffer, range.start, length, destination) {
                    return false;
                }
                true
            })
        };
        if mirrored {
            let _ = slot.memory.take_dirty_ranges(&mut state.store);
            slot.js_version = version;
            slot.wasm_version = slot.memory.data_version(&state.store);
        }
    }
}

fn sync_buffers_from_store(ctx: &mut Ctx, state: &mut WasmState, buffers: &mut [Option<Value>]) {
    for index in 0..state.memories.len() {
        let slot = &mut state.memories[index];
        let memory = slot.memory;
        let pages = memory.size(&state.store);
        let dirty_ranges = memory.take_dirty_ranges(&mut state.store);
        if pages != slot.buffer_pages {
            slot.buffer_pages = pages;
            slot.wasm_version = memory.data_version(&state.store);
            if let Some(buffer) = buffers.get_mut(index).and_then(Option::take) {
                ctx.detach_array_buffer(&buffer);
            }
            continue;
        }
        let version = memory.data_version(&state.store);
        if version == slot.wasm_version && dirty_ranges.is_empty() {
            continue;
        }
        let Some(buffer) = buffers.get(index).and_then(Option::as_ref) else {
            slot.wasm_version = version;
            continue;
        };
        let data = memory.data(&state.store);
        let ranges = if dirty_ranges.is_empty() {
            std::iter::once(0..data.len()).collect::<Vec<_>>()
        } else {
            dirty_ranges
        };
        let mirrored = ranges.iter().all(|range| {
            let Some(bytes) = data.get(range.clone()) else {
                return false;
            };
            ctx.array_buffer_set_range(buffer, range.start, bytes)
        });
        if mirrored {
            slot.wasm_version = version;
            slot.js_version = ctx.array_buffer_version(buffer).unwrap_or(slot.js_version);
        } else if let Some(buffer) = buffers.get_mut(index).and_then(Option::take) {
            ctx.detach_array_buffer(&buffer);
        }
    }
}

fn sync_active_buffers_to_js(ctx: &mut Ctx) {
    let Some(page) = page_wasm(ctx) else {
        return;
    };
    let mut buffers = page.buffers.borrow_mut();
    let _ = with_active_caller(|caller| {
        let _ = with_active_state(|state| {
            for index in 0..state.memories.len() {
                let slot = &mut state.memories[index];
                let memory = slot.memory;
                let pages = memory.size(&*caller);
                let dirty_ranges = memory.take_dirty_ranges(&mut *caller);
                if pages != slot.buffer_pages {
                    slot.buffer_pages = pages;
                    slot.wasm_version = memory.data_version(&*caller);
                    if let Some(buffer) = buffers.get_mut(index).and_then(Option::take) {
                        ctx.detach_array_buffer(&buffer);
                    }
                    continue;
                }
                let version = memory.data_version(&*caller);
                if version == slot.wasm_version && dirty_ranges.is_empty() {
                    continue;
                }
                let Some(buffer) = buffers.get(index).and_then(Option::as_ref) else {
                    slot.wasm_version = version;
                    continue;
                };
                let data = memory.data(&*caller);
                let ranges = if dirty_ranges.is_empty() {
                    std::iter::once(0..data.len()).collect::<Vec<_>>()
                } else {
                    dirty_ranges
                };
                let mirrored = ranges.iter().all(|range| {
                    let Some(bytes) = data.get(range.clone()) else {
                        return false;
                    };
                    ctx.array_buffer_set_range(buffer, range.start, bytes)
                });
                if mirrored {
                    slot.wasm_version = version;
                    slot.js_version = ctx.array_buffer_version(buffer).unwrap_or(slot.js_version);
                } else if let Some(buffer) = buffers.get_mut(index).and_then(Option::take) {
                    ctx.detach_array_buffer(&buffer);
                }
            }
        });
    });
}

fn sync_active_buffers_to_wasm(ctx: &mut Ctx) {
    let Some(page) = ctx
        .op_state()
        .get_mut::<HostState>()
        .map(|state| state.wasm.clone())
    else {
        return;
    };
    let buffers = page.buffers.borrow();
    let _ = with_active_caller(|caller| {
        let _ = with_active_state(|state| {
            for (index, slot) in state.memories.iter_mut().enumerate() {
                let Some(buffer) = buffers.get(index).and_then(Option::as_ref) else {
                    continue;
                };
                let version = ctx.array_buffer_version(buffer).unwrap_or(0);
                if version == slot.js_version {
                    continue;
                }
                let dirty_ranges = ctx
                    .take_array_buffer_dirty_ranges(buffer)
                    .unwrap_or_default();
                let data = slot.memory.data_mut(&mut *caller);
                let mirrored = if dirty_ranges.is_empty() {
                    let Some(bytes) = ctx.buffer_source_bytes(buffer, false) else {
                        continue;
                    };
                    if data.len() != bytes.len() {
                        false
                    } else {
                        data.copy_from_slice(&bytes);
                        true
                    }
                } else {
                    dirty_ranges.iter().all(|range| {
                        let Some(destination) = data.get_mut(range.clone()) else {
                            return false;
                        };
                        let Some(length) = range.end.checked_sub(range.start) else {
                            return false;
                        };
                        if !ctx.array_buffer_copy_range(buffer, range.start, length, destination) {
                            return false;
                        }
                        true
                    })
                };
                if mirrored {
                    let _ = slot.memory.take_dirty_ranges(&mut *caller);
                    slot.js_version = version;
                    slot.wasm_version = slot.memory.data_version(&*caller);
                }
            }
        });
    });
}

fn wasm_trap() -> wasmi::Error {
    wasmi::Error::new("WebAssembly host import raised a JavaScript exception")
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
                PreparedValue::Reference(value) => {
                    let funcs = with_active_state(|state| state.funcs.clone()).unwrap_or_default();
                    with_active_caller(|caller| build_ref(&funcs, caller, value))
                        .and_then(Result::ok)
                        .ok_or_else(|| type_error(ctx, "WebAssembly: invalid reference result"))?
                }
            };
            Ok(())
        }
        _ => {
            let global = ctx.global_this();
            let array = ctx.member_get(&global, "Array")?;
            let from = ctx.member_get(&array, "from")?;
            let values = ctx.invoke(from, array, &[returned])?;
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
                    PreparedValue::Reference(value) => {
                        let funcs =
                            with_active_state(|state| state.funcs.clone()).unwrap_or_default();
                        with_active_caller(|caller| build_ref(&funcs, caller, value))
                            .and_then(Result::ok)
                            .ok_or_else(|| {
                                type_error(ctx, "WebAssembly: invalid reference result")
                            })?
                    }
                };
            }
            Ok(())
        }
    }
}

fn make_import_func<C: wasmi::AsContextMut<Data = ()>>(
    context: &mut C,
    ty: wasmi::FuncType,
    token: u32,
    index: u32,
) -> wasmi::Func {
    let result_types = ty.results().to_vec();
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
        sync_active_buffers_to_js(ctx);
        let mut arguments = Vec::with_capacity(params.len());
        for value in params {
            let token = match value {
                wasmi::Val::FuncRef(wasmi::Ref::Val(function)) => {
                    let id = with_active_state(|state| state.register_func(*function)).unwrap_or(0);
                    make_exported_func(ctx, id)
                }
                wasmi::Val::ExternRef(wasmi::Ref::Val(reference)) => {
                    let id = reference
                        .data(&caller)
                        .downcast_ref::<usize>()
                        .copied()
                        .unwrap_or(0);
                    extern_get(ctx, id)
                }
                wasmi::Val::FuncRef(wasmi::Ref::Null) | wasmi::Val::ExternRef(wasmi::Ref::Null) => {
                    Ok(Value::Null)
                }
                value => numeric_wasm_to_js(ctx, value),
            };
            match token {
                Ok(value) => arguments.push(value),
                Err(error) => {
                    PENDING_THROW.with(|slot| *slot.borrow_mut() = Some(error));
                    return Err(wasm_trap());
                }
            }
        }
        let result = call_global(
            ctx,
            "__wasm_invoke_import",
            &[
                Value::Num(f64::from(token)),
                Value::Num(f64::from(index)),
                ctx.make_array(arguments),
            ],
        )
        .and_then(|returned| import_results(ctx, returned, &result_types, output));
        sync_active_buffers_to_wasm(ctx);
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                PENDING_THROW.with(|slot| *slot.borrow_mut() = Some(error));
                Err(wasm_trap())
            }
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
) -> Result<usize, (&'static str, String)> {
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
            _ => {
                return Err((
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
        Ok(Err(error)) => Err((error_kind(&error), error.to_string())),
        Err(_) => Err((
            "Link",
            "WebAssembly instantiation failed (unsupported feature)".to_string(),
        )),
    }
}

fn instantiate_active(
    module: &wasmi::Module,
    token: u32,
    bindings: &[ImportBinding],
) -> Result<usize, (&'static str, String)> {
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
            _ => {
                return Err((
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
        .ok_or((
            "Runtime",
            "WebAssembly: unavailable re-entrant state".to_string(),
        )),
        Ok(Err(error)) => Err((error_kind(&error), error.to_string())),
        Err(_) => Err((
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
    let bindings = match parse_imports(ctx, args.get(2), &import_types) {
        Ok(bindings) => bindings,
        Err((kind, message)) => return Ok(wasm_err(ctx, kind, message)),
    };
    let _context_guard = ContextGuard::set(ctx);
    let result = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let state = state_slot.get_or_insert_with(WasmState::new);
            sync_store_from_buffers(ctx, state, &page.buffers.borrow());
            let _state_guard = StateGuard::set(state);
            let result = instantiate_state(state, &module, token, &bindings);
            sync_buffers_from_store(ctx, state, &mut page.buffers.borrow_mut());
            result
        }
        Err(_) => {
            // WebAssembly Core §4.5 permits a host function to instantiate another
            // module. Reuse the active Caller so nested start functions and imports
            // execute in the same agent's associated store.
            sync_active_buffers_to_wasm(ctx);
            let result = instantiate_active(&module, token, &bindings);
            sync_active_buffers_to_js(ctx);
            result
        }
    };
    if let Some(error) = PENDING_THROW.with(|slot| slot.borrow_mut().take()) {
        return Err(error);
    }
    Ok(match result {
        Ok(id) => wasm_ok(ctx, Value::Num(id as f64)),
        Err((kind, message)) => wasm_err(ctx, kind, message),
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
                        .map(|value| state.register_memory(value)),
                    "table" => instance
                        .get_table(&state.store, &name)
                        .map(|value| state.register_table(value)),
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
                            wasmi::ValType::FuncRef => "anyfunc",
                            wasmi::ValType::ExternRef => "externref",
                            _ => "",
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
                let registered = with_active_caller(|caller| match kind {
                    "function" => instance
                        .get_func(&*caller, &name)
                        .and_then(|value| with_active_state(|state| state.register_func(value))),
                    "global" => instance
                        .get_global(&*caller, &name)
                        .and_then(|value| with_active_state(|state| state.register_global(value))),
                    "memory" => instance.get_memory(&*caller, &name).and_then(|value| {
                        let pages = value.size(&*caller);
                        with_active_state(|state| state.register_memory_with_pages(value, pages))
                    }),
                    "table" => instance
                        .get_table(&*caller, &name)
                        .and_then(|value| with_active_state(|state| state.register_table(value))),
                    _ => None,
                })
                .flatten();
                let Some(id) = registered else { continue };
                let auxiliary = if kind == "function" {
                    function_arity.to_string()
                } else if kind == "table" {
                    with_active_caller(|caller| {
                        instance.get_table(&*caller, &name).map(|table| {
                            match table.ty(&*caller).element() {
                                wasmi::ValType::FuncRef => "anyfunc",
                                wasmi::ValType::ExternRef => "externref",
                                _ => "",
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
    if parameters
        .iter()
        .chain(&results)
        .any(|ty| matches!(ty, wasmi::ValType::V128))
    {
        return Err(type_error(
            ctx,
            "WebAssembly: cannot call a function whose signature contains v128",
        ));
    }

    let arguments = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut prepared = Vec::with_capacity(parameters.len());
    for (index, ty) in parameters.iter().copied().enumerate() {
        let value = array_get(ctx, &arguments, index).unwrap_or(Value::Undefined);
        prepared.push(prepare_value(ctx, &value, ty)?);
    }

    let _context_guard = ContextGuard::set(ctx);
    let call: Result<Vec<OutputToken>, (&'static str, String)> = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => (|| -> Result<Vec<OutputToken>, (&'static str, String)> {
            let state = state_slot.get_or_insert_with(WasmState::new);
            sync_store_from_buffers(ctx, state, &page.buffers.borrow());
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
            let mut outputs: Vec<wasmi::Val> =
                results.iter().copied().map(wasmi::Val::default).collect();
            let _state_guard = StateGuard::set(state);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                function.call(&mut state.store, &inputs, &mut outputs)
            }));
            let tokens = match result {
                Ok(Ok(())) => {
                    let store = &state.store as *const wasmi::Store<()>;
                    outputs
                        .iter()
                        .map(|value| {
                            // SAFETY: the immutable store pointer remains valid for this
                            // extraction and does not overlap a mutable store operation.
                            extract_output(state, unsafe { &*store }, value)
                        })
                        .collect()
                }
                Ok(Err(error)) => return Err((error_kind(&error), error.to_string())),
                Err(_) => return Err(("Runtime", "WebAssembly trap".to_string())),
            };
            sync_buffers_from_store(ctx, state, &mut page.buffers.borrow_mut());
            Ok(tokens)
        })(),
        Err(_) => (|| -> Result<Vec<OutputToken>, (&'static str, String)> {
            // A host callback may have mutated a live Memory.buffer before re-entering wasm.
            // Commit those Data Block changes before the nested invocation (JS API §4.1).
            sync_active_buffers_to_wasm(ctx);
            let function = function_id.and_then(active_func).ok_or((
                "Runtime",
                "WebAssembly: unavailable re-entrant function".to_string(),
            ))?;
            let funcs = with_active_state(|state| state.funcs.clone()).unwrap_or_default();
            let mut inputs = Vec::with_capacity(prepared.len());
            for value in prepared {
                inputs.push(match value {
                    PreparedValue::Numeric(value) => value,
                    PreparedValue::Reference(value) => {
                        with_active_caller(|caller| build_ref(&funcs, caller, value))
                            .and_then(Result::ok)
                            .ok_or((
                                "Runtime",
                                "WebAssembly: invalid reference argument".to_string(),
                            ))?
                    }
                });
            }
            let mut outputs: Vec<wasmi::Val> =
                results.iter().copied().map(wasmi::Val::default).collect();
            let invoked = with_active_caller(|caller| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    function.call(&mut *caller, &inputs, &mut outputs)
                }))
            })
            .ok_or((
                "Runtime",
                "WebAssembly: unavailable re-entrant store".to_string(),
            ))?;
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
                    sync_active_buffers_to_js(ctx);
                    Ok(tokens)
                }
                Ok(Err(error)) => Err((error_kind(&error), error.to_string())),
                Err(_) => Err(("Runtime", "WebAssembly trap".to_string())),
            }
        })(),
    };

    if let Some(error) = PENDING_THROW.with(|slot| slot.borrow_mut().take()) {
        return Err(error);
    }
    let tokens = match call {
        Ok(tokens) => tokens,
        Err((kind, message)) => return Ok(wasm_err(ctx, kind, message)),
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
            let funcs = with_active_state(|state| state.funcs.clone()).unwrap_or_default();
            let initial = match initial {
                PreparedValue::Numeric(value) => value,
                PreparedValue::Reference(value) => {
                    with_active_caller(|caller| build_ref(&funcs, caller, value))
                        .and_then(Result::ok)
                        .ok_or_else(|| {
                            type_error(ctx, "WebAssembly.Global: invalid initial value")
                        })?
                }
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

pub(super) fn host_global_get(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Undefined);
    };
    let id = arg_id(args, 0);
    let token = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => state_slot.as_mut().and_then(|state| {
            id.and_then(|id| state.globals.get(id))
                .copied()
                .map(|global| {
                    let value = global.get(&state.store);
                    let store = &state.store as *const wasmi::Store<()>;
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
    let info = match page.state.try_borrow() {
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
    };
    let Some((ty, mutability)) = info else {
        return Ok(Value::Undefined);
    };
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
            let funcs = with_active_state(|state| state.funcs.clone()).unwrap_or_default();
            let value = match prepared {
                PreparedValue::Numeric(value) => value,
                PreparedValue::Reference(value) => {
                    with_active_caller(|caller| build_ref(&funcs, caller, value))
                        .and_then(Result::ok)
                        .ok_or_else(|| type_error(ctx, "WebAssembly.Global: invalid value"))?
                }
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
            state.register_memory(memory)
        }
        Err(_) => {
            let memory = with_active_caller(|caller| wasmi::Memory::new(caller, ty))
                .ok_or_else(|| range_error(ctx, "WebAssembly is unavailable"))?
                .map_err(|error| range_error(ctx, format!("WebAssembly.Memory: {error}")))?;
            let pages = with_active_caller(|caller| memory.size(&*caller)).unwrap_or(initial);
            with_active_state(|state| state.register_memory_with_pages(memory, pages))
                .ok_or_else(|| range_error(ctx, "WebAssembly is unavailable"))?
        }
    };
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
            sync_store_from_buffers(ctx, state, &page.buffers.borrow());
            id.and_then(|id| state.memories.get(id).map(|slot| slot.memory))
                .map(|memory| {
                    let result = memory.grow(&mut state.store, delta as u64);
                    if result.is_ok()
                        && let Some(id) = id
                    {
                        state.memories[id].buffer_pages = memory.size(&state.store);
                    }
                    result
                })
        }
        Err(_) => {
            sync_active_buffers_to_wasm(ctx);
            id.and_then(active_memory).and_then(|memory| {
                let result = with_active_caller(|caller| memory.grow(&mut *caller, delta as u64));
                if result.as_ref().is_some_and(Result::is_ok)
                    && let Some(id) = id
                {
                    let pages = with_active_caller(|caller| memory.size(&*caller)).unwrap_or(0);
                    let _ = with_active_state(|state| {
                        state.memories[id].buffer_pages = pages;
                    });
                }
                result
            })
        }
    };
    match result {
        Some(Ok(old)) => {
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
    let existing_buffer = {
        page.buffers
            .borrow()
            .get(id)
            .and_then(Option::as_ref)
            .cloned()
    };
    if let Some(buffer) = existing_buffer {
        let unchanged = match page.state.try_borrow() {
            Ok(state) => state.as_ref().is_some_and(|state| {
                state
                    .memories
                    .get(id)
                    .is_some_and(|slot| slot.memory.data_version(&state.store) == slot.wasm_version)
            }),
            Err(_) => {
                // A memory.buffer getter may run from an imported callback while the Wasmi store
                // is active. Bring only changed Wasmi memory into the existing JS object; the
                // helper also handles growth by detaching the old object.
                sync_active_buffers_to_js(ctx);
                page.buffers
                    .borrow()
                    .get(id)
                    .and_then(Option::as_ref)
                    .is_some_and(|current| ctx.object_addr(current) == ctx.object_addr(&buffer))
            }
        };
        if unchanged {
            return Ok(buffer);
        }
    }
    let bytes = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let Some(state) = state_slot.as_mut() else {
                return Ok(Value::Undefined);
            };
            let Some(memory) = state.memories.get(id).map(|slot| slot.memory) else {
                return Ok(Value::Undefined);
            };
            let pages = memory.size(&state.store);
            if pages != state.memories[id].buffer_pages {
                state.memories[id].buffer_pages = pages;
                detach_buffer(ctx, &mut page.buffers.borrow_mut(), id);
            }
            memory.data(&state.store).to_vec()
        }
        Err(_) => {
            let Some(memory) = active_memory(id) else {
                return Ok(Value::Undefined);
            };
            let Some(bytes) = with_active_caller(|caller| memory.data(&*caller).to_vec()) else {
                return Ok(Value::Undefined);
            };
            let pages = with_active_caller(|caller| memory.size(&*caller)).unwrap_or(0);
            let changed = with_active_state(|state| {
                let changed = state.memories[id].buffer_pages != pages;
                state.memories[id].buffer_pages = pages;
                changed
            })
            .unwrap_or(false);
            if changed {
                detach_buffer(ctx, &mut page.buffers.borrow_mut(), id);
            }
            bytes
        }
    };
    if let Some(buffer) = page
        .buffers
        .borrow()
        .get(id)
        .and_then(Option::as_ref)
        .cloned()
        && ctx.array_buffer_set_bytes(&buffer, &bytes)
    {
        let version = ctx.array_buffer_version(&buffer).unwrap_or(0);
        if let Ok(mut state) = page.state.try_borrow_mut()
            && let Some(state) = state.as_mut()
            && let Some(slot) = state.memories.get_mut(id)
        {
            let _ = slot.memory.take_dirty_ranges(&mut state.store);
            slot.wasm_version = slot.memory.data_version(&state.store);
            slot.js_version = version;
        }
        return Ok(buffer);
    }
    let buffer = ctx.make_host_keyed_array_buffer(&bytes)?;
    let mut buffers = page.buffers.borrow_mut();
    if buffers.len() <= id {
        buffers.resize(id + 1, None);
    }
    buffers[id] = Some(buffer.clone());
    let version = ctx.array_buffer_version(&buffer).unwrap_or(0);
    if let Ok(mut state) = page.state.try_borrow_mut()
        && let Some(state) = state.as_mut()
        && let Some(slot) = state.memories.get_mut(id)
    {
        let _ = slot.memory.take_dirty_ranges(&mut state.store);
        slot.wasm_version = slot.memory.data_version(&state.store);
        slot.js_version = version;
    }
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
    let ty = wasmi::TableType::new(element, initial, maximum);
    let id = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let state = state_slot.get_or_insert_with(WasmState::new);
            let initial_value = build_ref(&state.funcs, &mut state.store, prepared)
                .map_err(|()| type_error(ctx, "WebAssembly.Table: invalid initial value"))?;
            let table = wasmi::Table::new(&mut state.store, ty, initial_value)
                .map_err(|error| range_error(ctx, format!("WebAssembly.Table: {error}")))?;
            state.register_table(table)
        }
        Err(_) => {
            let funcs = with_active_state(|state| state.funcs.clone()).unwrap_or_default();
            let initial_value = with_active_caller(|caller| build_ref(&funcs, caller, prepared))
                .and_then(Result::ok)
                .ok_or_else(|| type_error(ctx, "WebAssembly.Table: invalid initial value"))?;
            let table = with_active_caller(|caller| wasmi::Table::new(caller, ty, initial_value))
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
    let index = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0);
    if index < 0.0 || !index.is_finite() || index.fract() != 0.0 {
        return Err(range_error(
            ctx,
            "WebAssembly.Table.get: index is out of bounds",
        ));
    }
    let id = arg_id(args, 0);
    let token = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let Some(state) = state_slot.as_mut() else {
                return Err(range_error(ctx, "WebAssembly.Table.get: unknown table"));
            };
            let value = id
                .and_then(|id| state.tables.get(id))
                .and_then(|table| table.get(&state.store, index as u64));
            value.map(|value| {
                let store = &state.store as *const wasmi::Store<()>;
                // SAFETY: immutable extraction only.
                extract_output(state, unsafe { &*store }, &value)
            })
        }
        Err(_) => id.and_then(active_table).and_then(|table| {
            with_active_caller(|caller| {
                table.get(&*caller, index as u64).and_then(|value| {
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
                .map(|table| table.ty(&state.store).element())
        }),
        Err(_) => id
            .and_then(active_table)
            .and_then(|table| with_active_caller(|caller| table.ty(&*caller).element())),
    }
}

pub(super) fn host_table_set(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(page) = page_wasm(ctx) else {
        return Ok(Value::Undefined);
    };
    let id = arg_id(args, 0);
    let index = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0);
    if index < 0.0 || !index.is_finite() || index.fract() != 0.0 {
        return Err(range_error(
            ctx,
            "WebAssembly.Table.set: index is out of bounds",
        ));
    }
    let Some(element) = table_info(&page, id) else {
        return Err(range_error(ctx, "WebAssembly.Table.set: unknown table"));
    };
    let prepared = prepare_ref(ctx, args.get(2).unwrap_or(&Value::Undefined), element)?;
    let result: Option<Result<(), String>> = match page.state.try_borrow_mut() {
        Ok(mut state_slot) => {
            let Some(state) = state_slot.as_mut() else {
                return Err(range_error(ctx, "WebAssembly.Table.set: unknown table"));
            };
            let value = build_ref(&state.funcs, &mut state.store, prepared)
                .map_err(|()| type_error(ctx, "WebAssembly.Table.set: invalid funcref"))?;
            id.and_then(|id| state.tables.get(id))
                .copied()
                .map(|table| {
                    table
                        .set(&mut state.store, index as u64, value)
                        .map_err(|error| error.to_string())
                })
        }
        Err(_) => {
            let funcs = with_active_state(|state| state.funcs.clone()).unwrap_or_default();
            id.and_then(active_table).and_then(|table| {
                with_active_caller(|caller| match build_ref(&funcs, caller, prepared) {
                    Ok(value) => table
                        .set(caller, index as u64, value)
                        .map_err(|error| error.to_string()),
                    Err(()) => Err("invalid funcref".to_string()),
                })
            })
        }
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
            let value = build_ref(&state.funcs, &mut state.store, prepared)
                .map_err(|()| type_error(ctx, "WebAssembly.Table.grow: invalid funcref"))?;
            id.and_then(|id| state.tables.get(id))
                .copied()
                .map(|table| {
                    table
                        .grow(&mut state.store, delta as u64, value)
                        .map_err(|error| error.to_string())
                })
        }
        Err(_) => {
            let funcs = with_active_state(|state| state.funcs.clone()).unwrap_or_default();
            id.and_then(active_table).and_then(|table| {
                with_active_caller(|caller| match build_ref(&funcs, caller, prepared) {
                    Ok(value) => table
                        .grow(caller, delta as u64, value)
                        .map_err(|error| error.to_string()),
                    Err(()) => Err("invalid funcref".to_string()),
                })
            })
        }
    };
    match result {
        Some(Ok(old)) => Ok(Value::Num(old as f64)),
        Some(Err(error)) => Err(range_error(ctx, format!("WebAssembly.Table.grow: {error}"))),
        None => Err(range_error(ctx, "WebAssembly.Table.grow: unknown table")),
    }
}
