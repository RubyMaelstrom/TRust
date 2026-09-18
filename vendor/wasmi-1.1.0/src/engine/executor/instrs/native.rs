use super::Executor;
use crate::{
    core::UntypedVal,
    engine::native_jit::{NativeMemory, NativeRegion, MAX_CANDIDATES, MAX_GLOBALS},
    store::StoreInner,
};
use rustc_hash::FxHashMap;
use std::sync::Arc;

#[derive(Debug, Default)]
struct Entry {
    visits: u8,
    region: Option<Arc<NativeRegion>>,
}

#[derive(Debug)]
pub(super) struct NativeState {
    previous: usize,
    // Keep each candidate's hotness until this execution returns. A direct
    // pointer-indexed cache can starve hot functions when aligned code
    // allocations collide, repeatedly discarding their visit counts.
    // Keys are internal instruction addresses, not guest-provided strings.
    // Keep this bounded lookup cheap enough for every control-flow entry.
    entries: FxHashMap<usize, Entry>,
}

impl Default for NativeState {
    fn default() -> Self {
        Self {
            previous: usize::MAX,
            entries: FxHashMap::default(),
        }
    }
}

impl Executor<'_> {
    #[inline]
    pub(super) fn try_native(&mut self, store: &mut StoreInner) -> bool {
        if !self.code_map.native_jit.enabled {
            return false;
        }
        let address = self.ip.get() as *const _ as usize;
        let previous = core::mem::replace(&mut self.native.previous, address);
        // Observe every control-flow entry, including calls and forward
        // branches. Hot leaf functions can consume most of a workload without
        // containing a backwards branch of their own.
        if address == previous.wrapping_add(core::mem::size_of::<crate::ir::Op>()) {
            return false;
        }
        self.enter_native(store, address)
    }

    #[cold]
    fn enter_native(&mut self, store: &mut StoreInner, address: usize) -> bool {
        let entry = if self.native.entries.len() >= MAX_CANDIDATES {
            let Some(entry) = self.native.entries.get_mut(&address) else {
                return false;
            };
            entry
        } else {
            self.native.entries.entry(address).or_default()
        };
        if entry.visits < 32 {
            entry.visits += 1;
            return false;
        }
        if entry.visits == 32 {
            entry.visits += 1;
            entry.region = self
                .code_map
                .native_jit
                .get_or_compile(self.code_map, address);
        }
        let Some(region) = &entry.region else {
            return false;
        };
        let mut globals = [core::ptr::null_mut::<UntypedVal>(); MAX_GLOBALS];
        for (ptr, global) in globals.iter_mut().zip(&region.globals) {
            // SAFETY: the interpreter refreshes the instance/global cache at
            // every boundary capable of moving a store or switching instance.
            *ptr = unsafe { self.cache.global_at(store, *global).as_mut_ptr() };
        }
        // SAFETY: the region belongs to this pinned function; the frame and
        // global addresses are fresh. Native regions cannot call or grow them.
        let bytes = unsafe { self.cache.memory.data_mut() };
        let mut memory = NativeMemory {
            bytes: bytes.as_mut_ptr(),
            len: bytes.len(),
            dirty_start: usize::MAX,
            dirty_end: 0,
        };
        let next = unsafe { region.execute(self.sp.as_mut_ptr(), globals.as_ptr(), &mut memory) };
        if memory.dirty_start < memory.dirty_end {
            self.mark_memory_dirty(
                store,
                crate::ir::index::Memory::from(0),
                memory.dirty_start as u64,
                memory.dirty_end - memory.dirty_start,
            );
        }
        self.ip = super::InstructionPtr::new(next);
        self.native.previous = usize::MAX;
        // If the first instruction cannot complete natively (for example an
        // out-of-bounds access), execute it in Wasmi to preserve its exact trap.
        next as usize != address
    }
}
