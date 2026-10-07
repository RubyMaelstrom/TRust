//! TRust: entering native regions at the interpreter's control-flow entries.
//!
//! Branches, calls and returns observe their target operator. Once an operator
//! has been entered often enough, the engine compiles the region starting at it
//! (see `engine::native_jit`) and later entries run that region natively. The
//! region continues at the address it returns, which may enter another region.

use super::{
    args::Args,
    state::{Freg32, Freg64, Ireg},
    utils::LoadEntity as _,
};
use crate::{
    engine::native_jit::{MAX_CANDIDATES, MAX_GLOBALS, NativeMemory, NativeRegion, NativeRegs},
    store::PrunedStore,
};
use alloc::sync::Arc;
use rustc_hash::FxHashMap;

/// Bounds the regions chained without returning to the interpreter's dispatch.
const MAX_CHAINED: usize = 8;

#[derive(Debug, Default)]
struct Entry {
    visits: u8,
    region: Option<Arc<NativeRegion>>,
}

/// The handler state of one native entry, held by the store during the entry.
#[derive(Debug, Copy, Clone)]
struct EntryArgs(Args);

// SAFETY: `EntryArgs` is only present while one native entry runs on the thread executing
//         the store; it is taken before the entry returns to its handler.
unsafe impl Send for EntryArgs {}
// SAFETY: see `Send`; no shared access to it exists.
unsafe impl Sync for EntryArgs {}

/// The native region entries observed by one execution.
#[derive(Debug, Default)]
pub struct NativeState {
    // Keys are internal operator addresses, not guest-provided strings. Keep
    // this bounded lookup cheap enough for every control-flow entry.
    entries: FxHashMap<usize, Entry>,
    /// The handler state passed through the store, see [`enter`].
    args: Option<EntryArgs>,
}

impl NativeState {
    /// Resets `self` for a new execution.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.args = None;
    }
}

/// Observes the control-flow entry at `args.ip` and runs its native region if any.
///
/// # Note
///
/// Handlers dispatch to their successor with a sibling call, which LLVM only performs if
/// no address of a handler local has escaped. The handler state therefore travels through
/// the store instead of a reference to the handler's `args`.
#[inline(always)]
pub fn enter(store: &mut PrunedStore, args: &mut Args) {
    if store.inner().native_jit_enabled() {
        store.stack_mut().native_mut().args = Some(EntryArgs(*args));
        enter_native(store);
        if let Some(EntryArgs(updated)) = store.stack_mut().native_mut().args.take() {
            *args = updated;
        }
    }
}

#[cold]
#[inline(never)]
fn enter_native(store: &mut PrunedStore) {
    let Some(EntryArgs(mut args)) = store.stack_mut().native_mut().args else {
        return;
    };
    enter_regions(store, &mut args);
    store.stack_mut().native_mut().args = Some(EntryArgs(args));
}

fn enter_regions(store: &mut PrunedStore, args: &mut Args) {
    for _ in 0..MAX_CHAINED {
        let address = args.ip.addr();
        let Some(region) = lookup(store, address) else {
            return;
        };
        // SAFETY: the region is owned by this execution's entries and by the engine, and
        //         neither is modified while it runs: native code cannot call the host.
        let next = run(store, args, unsafe { &*region });
        if next == address {
            // The first operator cannot complete natively (for example an out-of-bounds
            // access); the interpreter executes it to preserve its exact trap.
            return;
        }
        // SAFETY: regions only return operator addresses of their own function.
        args.ip = unsafe { args.ip.offset(next.wrapping_sub(address) as isize) };
    }
}

/// Returns the region at `address` once it is hot enough to have been compiled.
fn lookup(store: &mut PrunedStore, address: usize) -> Option<*const NativeRegion> {
    let (exec, engine) = store.inner_mut().exec_and_engine_mut();
    let (jit, code_map) = engine.native_jit();
    let entries = &mut exec.stack_mut().native_mut().entries;
    let entry = if entries.len() >= MAX_CANDIDATES {
        entries.get_mut(&address)?
    } else {
        entries.entry(address).or_default()
    };
    if entry.visits == 0 {
        if let Some(region) = jit.observe(code_map, address) {
            entry.visits = 33;
            entry.region = region;
        }
    }
    if entry.visits < 32 {
        entry.visits += 1;
        return None;
    }
    if entry.visits == 32 {
        entry.visits += 1;
        entry.region = jit.get_or_compile(code_map, address);
    }
    entry.region.as_ref().map(Arc::as_ptr)
}

/// Runs `region` on the state of `args` and returns the address of the next operator.
fn run(store: &mut PrunedStore, args: &mut Args, region: &NativeRegion) -> usize {
    let mut globals = [core::ptr::null_mut::<u64>(); MAX_GLOBALS];
    for (ptr, global) in globals.iter_mut().zip(&region.globals) {
        // SAFETY: the global addresses stem from operators of the executing instance, whose
        //         cache is warmed; the value pointers stay valid since native code cannot
        //         relocate the store.
        let entity = unsafe { args.instance.load_entity_ptr(*global) };
        *ptr = unsafe { (*entity.as_ptr()).get_raw_ptr() }
            .as_ptr()
            .cast::<u64>();
    }
    let mut memory = NativeMemory {
        bytes: args.mem0_ptr.as_ptr(),
        len: args.mem0_len.get(),
        dirty_start: usize::MAX,
        dirty_end: 0,
    };
    let mut regs = NativeRegs {
        ireg: u64::from(args.ireg),
        freg32: u64::from(f32::from(args.freg32).to_bits()),
        freg64: f64::from(args.freg64).to_bits(),
    };
    // SAFETY: the region belongs to the function of the current frame, whose cells,
    //         globals and default memory are passed fresh. Native regions cannot call,
    //         grow memory or otherwise move them.
    let next = unsafe {
        region.execute(
            args.sp.as_ptr().cast::<u64>(),
            globals.as_ptr(),
            &mut memory,
            &mut regs,
        )
    };
    if memory.dirty_start < memory.dirty_end {
        args.mark_mem0_dirty(
            store,
            memory.dirty_start as u64,
            memory.dirty_end - memory.dirty_start,
        );
    }
    args.ireg = Ireg::from(regs.ireg);
    args.freg32 = Freg32::from(f32::from_bits(regs.freg32 as u32));
    args.freg64 = Freg64::from(f64::from_bits(regs.freg64));
    next
}
