//! TRust: the exception instances of a [`Store`](crate::Store).
//!
//! WebAssembly Core 3.0, `exec/runtime.rst` "Exception Instances" and `exec/modules.rst`
//! `alloc-exception` (local snapshots of the spec repository 37d6b059 and the exception-handling
//! repository af287a73): the store holds exception instances, each made of a tag address and
//! field values, which exception references (`exnref`) address. `throw_ref` raises the
//! referenced exception again and a `catch_ref` or `catch_all_ref` clause hands out a reference
//! to the exception it catches.
//!
//! # Note
//!
//! An exception in flight is owned by the executor's exception state and only enters this
//! store when a reference to it is created, so `throw` and catch clauses without references
//! allocate nothing here. Exception references have no identity a Wasm program can observe
//! (`exnref` is no `eqref`), but one exception keeps its address while it is caught and
//! thrown again, as the specification describes.
//!
//! The store frees exceptions that became unreachable with a non-moving mark and sweep
//! collection, so that Wasm code catching references in a loop runs in bounded memory. Its
//! roots are the `exnref` globals and tables of the store, the executing [`Stack`] (scanned
//! conservatively: any cell that encodes the reference of a live exception keeps it) and the
//! exceptions referenced from outside the store. Those are either pinned for good (references
//! read by the host and those held by a suspended resumable call) or held: an embedder that
//! tracks the lifetime of its references, such as a JavaScript `WebAssembly.Exception` object
//! ([`ExnRef::release`]), counts holds that it releases again. The fields of reachable
//! exceptions are scanned conservatively as well. A collection only runs while no enclosing
//! execution is parked behind a host function, since parked stacks are not reachable from the
//! store.
//!
//! The exception instances are identified by their tag (a [`Tag`] handle, WebAssembly Core 3.0
//! `exec/runtime.rst` "Exception Instances"), never by the instance that threw them.
//!
//! [`Stack`]: crate::engine::Stack
//! [`ExnRef::release`]: crate::ExnRef::release

use crate::{Tag, core::RawRef, engine::Cell};
use alloc::{boxed::Box, vec::Vec};
use core::{
    mem,
    num::NonZero,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

/// An odd multiplier that spreads the encodings of exception references over the 32-bit range.
///
/// # Note
///
/// The bijection keeps encodings nonzero and makes integers and other references unlikely to
/// alias a live exception in the conservative scans, so they rarely retain dead exceptions.
const SPREAD: u32 = 0x9E37_79B1;

/// The multiplicative inverse of [`SPREAD`] modulo 2^32.
const SPREAD_INVERSE: u32 = inverse(SPREAD);

/// Returns the multiplicative inverse of the odd `value` modulo 2^32 (Newton's iteration).
const fn inverse(value: u32) -> u32 {
    // An odd `value` is its own inverse modulo 8; every step doubles the correct low bits.
    let mut inverse = value;
    let mut step = 0;
    while step < 4 {
        inverse = inverse.wrapping_mul(2_u32.wrapping_sub(value.wrapping_mul(inverse)));
        step += 1;
    }
    inverse
}

const _: () = assert!(SPREAD.wrapping_mul(SPREAD_INVERSE) == 1);

/// The fewest live exceptions at which a collection may run.
const MIN_COLLECTION_THRESHOLD: usize = 256;

/// An exception instance: its tag address and field values.
#[derive(Debug)]
pub struct ExnEntity {
    /// The tag of the exception.
    tag: Tag,
    /// The tag fields in cells.
    fields: Box<[Cell]>,
    /// `true` if a reference to this exception exists outside the store for good.
    pinned: AtomicBool,
    /// The number of releasable embedder holds of this exception.
    holds: AtomicU32,
}

impl ExnEntity {
    /// Creates a new [`ExnEntity`] of `tag` with the tag fields `fields`.
    pub fn new(tag: Tag, fields: Vec<Cell>) -> Self {
        Self {
            tag,
            fields: fields.into_boxed_slice(),
            pinned: AtomicBool::new(false),
            holds: AtomicU32::new(0),
        }
    }

    /// Returns the tag of the exception.
    pub fn tag(&self) -> Tag {
        self.tag
    }

    /// Returns `true` if the store must keep the exception regardless of Wasm roots.
    fn is_external_root(&self, holds: bool) -> bool {
        self.pinned.load(Ordering::Relaxed) || (holds && self.holds.load(Ordering::Relaxed) != 0)
    }

    /// Returns the tag fields.
    pub fn fields(&self) -> &[Cell] {
        &self.fields
    }
}

/// The exception instances of a store.
#[derive(Debug)]
pub struct ExnStore {
    /// The exceptions by index; `None` for free slots.
    slots: Vec<Option<ExnEntity>>,
    /// The free slot indices, lowest last.
    free: Vec<u32>,
    /// The number of exceptions in `slots`.
    live: usize,
    /// The number of live exceptions at which the next collection runs.
    threshold: usize,
    /// The mark bits of a collection, one per slot.
    marks: Vec<u64>,
    /// The marked exceptions whose fields remain to be scanned.
    worklist: Vec<u32>,
}

impl Default for ExnStore {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            live: 0,
            threshold: MIN_COLLECTION_THRESHOLD,
            marks: Vec::new(),
            worklist: Vec::new(),
        }
    }
}

impl ExnStore {
    /// Returns the reference encoding the exception at slot `index`.
    fn encode(index: usize) -> RawRef {
        let Ok(index) = u32::try_from(index) else {
            unreachable!("exception slot indices fit 32 bits")
        };
        RawRef::from(index.wrapping_add(1).wrapping_mul(SPREAD))
    }

    /// Returns the slot index of the live exception referenced by `raw`, if any.
    fn index_of(&self, raw: u32) -> Option<usize> {
        let index = raw.wrapping_mul(SPREAD_INVERSE).wrapping_sub(1);
        let index = usize::try_from(index).ok()?;
        match self.slots.get(index) {
            Some(Some(_)) => Some(index),
            _ => None,
        }
    }

    /// Returns the number of live exceptions.
    pub fn live(&self) -> usize {
        self.live
    }

    /// Returns the fields of all live exceptions.
    pub fn fields(&self) -> impl Iterator<Item = &[Cell]> {
        self.slots.iter().flatten().map(ExnEntity::fields)
    }

    /// Returns the exception referenced by `raw`, if it is live.
    pub fn get(&self, raw: RawRef) -> Option<&ExnEntity> {
        let index = self.index_of(u32::from(raw))?;
        self.slots[index].as_ref()
    }

    /// Allocates `entity` and returns its reference.
    ///
    /// Returns `None` if the store's exception address space is exhausted.
    pub fn alloc(&mut self, entity: ExnEntity) -> Option<NonZero<u32>> {
        let index = match self.free.pop() {
            Some(index) => {
                let index = index as usize;
                debug_assert!(self.slots[index].is_none());
                self.slots[index] = Some(entity);
                index
            }
            None => {
                let index = self.slots.len();
                if index >= u32::MAX as usize {
                    return None;
                }
                self.slots.push(Some(entity));
                index
            }
        };
        self.live += 1;
        NonZero::new(u32::from(Self::encode(index)))
    }

    /// Pins the exception referenced by `raw`, which keeps it alive as long as the store.
    ///
    /// Does nothing for null or dangling references.
    pub fn pin(&self, raw: RawRef) {
        if let Some(entity) = self.get(raw) {
            entity.pinned.store(true, Ordering::Relaxed);
        }
    }

    /// Adds a releasable embedder hold to the exception referenced by `raw`.
    ///
    /// Returns `false` for null or dangling references.
    pub fn hold(&self, raw: RawRef) -> bool {
        let Some(entity) = self.get(raw) else {
            return false;
        };
        // Saturate instead of wrapping: an exhausted count keeps the exception for good.
        update_holds(&entity.holds, |holds| holds.checked_add(1));
        true
    }

    /// Releases one embedder hold of the exception referenced by `raw`, see [`ExnStore::hold`].
    ///
    /// Does nothing for null or dangling references and exceptions without holds.
    pub fn release(&self, raw: RawRef) {
        if let Some(entity) = self.get(raw) {
            update_holds(&entity.holds, |holds| match holds {
                // A saturated count can no longer be balanced.
                0 | u32::MAX => None,
                holds => Some(holds - 1),
            });
        }
    }

    /// Pins every live exception that a cell of `cells` references.
    pub fn pin_cells(&self, cells: impl IntoIterator<Item = u64>) {
        for cell in cells {
            if let Ok(raw) = u32::try_from(cell) {
                self.pin(RawRef::from(raw));
            }
        }
    }

    /// Returns `true` if enough exceptions were allocated since the last collection.
    pub fn wants_collection(&self) -> bool {
        self.live >= self.threshold
    }

    /// Marks the exceptions reachable from the roots `trace` marks and from the pinned ones,
    /// and from the held ones if `holds` is `true`, into `marks`; returns the scanned cells.
    fn mark(
        &self,
        marks: &mut Vec<u64>,
        worklist: &mut Vec<u32>,
        trace: impl FnOnce(&mut ExnMarker<'_>),
        holds: bool,
    ) -> usize {
        let words = self.slots.len().div_ceil(64);
        marks.clear();
        marks.resize(words, 0);
        worklist.clear();
        let mut marker = ExnMarker {
            store: &self.slots,
            marks,
            worklist,
            scanned: 0,
        };
        trace(&mut marker);
        for (index, slot) in self.slots.iter().enumerate() {
            if slot
                .as_ref()
                .is_some_and(|entity| entity.is_external_root(holds))
            {
                marker.mark_index(index);
            }
        }
        // Exceptions may hold references in their fields: scan the marked ones transitively.
        while let Some(index) = marker.worklist.pop() {
            let Some(entity) = &marker.store[index as usize] else {
                continue;
            };
            for field in entity.fields.iter() {
                marker.scanned += 1;
                marker.mark_cell(u64::from(*field));
            }
        }
        marker.scanned
    }

    /// Reports the live exceptions that the roots `trace` marks or the pinned exceptions reach,
    /// directly or through the fields of other exceptions, to `reachable`, and every field of a
    /// live exception that references a live exception to `edge` (conservatively, as cells).
    ///
    /// # Note
    ///
    /// Releasable holds are no roots here: this tells an embedder which of the exceptions it
    /// holds the store would keep without its holds, and which held exceptions keep others.
    pub fn visit_references(
        &self,
        trace: impl FnOnce(&mut ExnMarker<'_>),
        mut reachable: impl FnMut(RawRef),
        mut edge: impl FnMut(RawRef, RawRef),
    ) {
        let mut marks = Vec::new();
        let mut worklist = Vec::new();
        self.mark(&mut marks, &mut worklist, trace, false);
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(entity) = slot else { continue };
            let raw = Self::encode(index);
            if marks[index / 64] & (1 << (index % 64)) != 0 {
                reachable(raw);
            }
            for field in entity.fields.iter() {
                let Ok(field) = u32::try_from(u64::from(*field)) else {
                    continue;
                };
                if let Some(target) = self.index_of(field) {
                    edge(raw, Self::encode(target));
                }
            }
        }
    }

    /// Frees all exceptions that are neither pinned, held nor reachable from the roots `trace`
    /// marks.
    pub fn collect(&mut self, trace: impl FnOnce(&mut ExnMarker<'_>)) {
        let mut marks = mem::take(&mut self.marks);
        let mut worklist = mem::take(&mut self.worklist);
        let scanned = self.mark(&mut marks, &mut worklist, trace, true);
        // Sweep, trim the trailing free slots and list the others for reuse, lowest last.
        self.free.clear();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let marked = marks[index / 64] & (1 << (index % 64)) != 0;
            if slot.is_some() && !marked {
                *slot = None;
                self.live -= 1;
            }
        }
        self.marks = marks;
        self.worklist = worklist;
        while let Some(None) = self.slots.last() {
            self.slots.pop();
        }
        for (index, slot) in self.slots.iter().enumerate().rev() {
            if slot.is_none() {
                self.free.push(index as u32);
            }
        }
        if self.slots.capacity() > 4 * self.slots.len().max(MIN_COLLECTION_THRESHOLD) {
            self.slots.shrink_to(2 * self.slots.len());
            self.free.shrink_to_fit();
            self.marks = Vec::new();
            self.worklist = Vec::new();
        }
        // Amortize the next collection over at least as many allocations as cells it scans.
        let work = scanned.saturating_add(self.slots.len());
        let headroom = MIN_COLLECTION_THRESHOLD.max(self.live).max(work / 4);
        self.threshold = self.live.saturating_add(headroom);
    }
}

/// Atomically replaces the hold count `holds` by `update(holds)` unless that is `None`.
fn update_holds(holds: &AtomicU32, update: impl Fn(u32) -> Option<u32>) {
    let mut current = holds.load(Ordering::Relaxed);
    while let Some(next) = update(current) {
        match holds.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

/// Marks the exceptions referenced by the roots of a collection.
#[derive(Debug)]
pub struct ExnMarker<'a> {
    /// The slots of the collected store.
    store: &'a [Option<ExnEntity>],
    /// The mark bits, one per slot.
    marks: &'a mut Vec<u64>,
    /// The marked exceptions whose fields remain to be scanned.
    worklist: &'a mut Vec<u32>,
    /// The number of cells scanned so far.
    scanned: usize,
}

impl ExnMarker<'_> {
    /// Marks the exception at slot `index` if it is live and not yet marked.
    fn mark_index(&mut self, index: usize) {
        if self.store.get(index).is_none_or(Option::is_none) {
            return;
        }
        let (word, bit) = (index / 64, 1 << (index % 64));
        if self.marks[word] & bit == 0 {
            self.marks[word] |= bit;
            self.worklist.push(index as u32);
        }
    }

    /// Marks the exception referenced by `raw`, if any.
    pub fn mark_ref(&mut self, raw: RawRef) {
        let index = u32::from(raw).wrapping_mul(SPREAD_INVERSE).wrapping_sub(1);
        self.mark_index(index as usize);
    }

    /// Marks the exception that the value `cell` would reference, if any.
    ///
    /// # Note
    ///
    /// This is the conservative scan of untyped cells: references occupy the low 32 bits of
    /// a cell and leave its high bits zero.
    pub fn mark_cell(&mut self, cell: u64) {
        if let Ok(raw) = u32::try_from(cell) {
            self.mark_ref(RawRef::from(raw));
        }
    }

    /// Conservatively marks the exceptions referenced by `cells`.
    pub fn mark_cells(&mut self, cells: impl IntoIterator<Item = u64>) {
        for cell in cells {
            self.scanned += 1;
            self.mark_cell(cell);
        }
    }

    /// Precisely marks the exceptions referenced by `refs`.
    pub fn mark_refs(&mut self, refs: impl IntoIterator<Item = RawRef>) {
        for raw in refs {
            self.scanned += 1;
            self.mark_ref(raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Nullable;

    fn exn(fields: &[u64]) -> ExnEntity {
        use crate::{Handle as _, RawHandle, store::AsStoreId as _};
        // The arena never resolves the tag of an exception, so any tag handle does.
        let tag = Tag::from_raw(crate::store::id::StoreId::new().wrap(RawHandle::new(0)));
        ExnEntity::new(tag, fields.iter().copied().map(Cell::from).collect())
    }

    #[test]
    fn encodings_are_nonzero_and_round_trip() {
        let mut store = ExnStore::default();
        for index in 0..1000 {
            let raw = store.alloc(exn(&[index])).unwrap();
            assert_eq!(store.index_of(raw.get()), Some(index as usize));
        }
        assert!(store.get(RawRef::null()).is_none());
        assert_eq!(store.live(), 1000);
    }

    #[test]
    fn collection_keeps_roots_pins_and_their_fields() {
        let mut store = ExnStore::default();
        let a = store.alloc(exn(&[])).unwrap();
        let b = store.alloc(exn(&[u64::from(a.get())])).unwrap();
        let c = store.alloc(exn(&[])).unwrap();
        let d = store.alloc(exn(&[])).unwrap();
        store.pin(RawRef::from(d.get()));
        store.collect(|marker| marker.mark_cells([u64::from(b.get()), 7, u64::MAX]));
        assert!(
            store.get(RawRef::from(a.get())).is_some(),
            "field of a root"
        );
        assert!(store.get(RawRef::from(b.get())).is_some(), "root");
        assert!(store.get(RawRef::from(c.get())).is_none(), "unreachable");
        assert!(store.get(RawRef::from(d.get())).is_some(), "pinned");
        assert_eq!(store.live(), 3);
        // The freed slot is reused.
        let e = store.alloc(exn(&[])).unwrap();
        assert_eq!(e, c);
        store.collect(|_| {});
        assert_eq!(store.live(), 1);
        assert!(store.get(RawRef::from(d.get())).is_some());
    }

    /// Instantiates `wat` with the host function `host.call(i32)`, which calls the export
    /// `churn` of the instance with its argument.
    fn instantiate(wat: &str) -> (crate::Store<()>, crate::Instance) {
        use crate::{Caller, Engine, Extern, Linker, Module, Store, Val};
        let engine = Engine::default();
        let mut store = Store::new(&engine, ());
        let mut linker = <Linker<()>>::new(&engine);
        linker
            .func_wrap("host", "call", |mut caller: Caller<'_, ()>, n: i32| {
                let Some(Extern::Func(churn)) = caller.get_export("churn") else {
                    panic!("missing export `churn`")
                };
                churn.call(&mut caller, &[Val::I32(n)], &mut []).unwrap();
            })
            .unwrap();
        let module = Module::new(&engine, wat::parse_str(wat).unwrap()).unwrap();
        let instance = linker.instantiate_and_start(&mut store, &module).unwrap();
        (store, instance)
    }

    const MODULE: &str = r#"
        (module
          (import "host" "call" (func $host (param i32)))
          (tag $e (param i32))
          (global $g (export "g") (mut exnref) (ref.null exn))
          (func $catch (param i32) (result exnref)
            (block $h (result exnref)
              (try_table (catch_all_ref $h) (throw $e (local.get 0)))
              (unreachable)))
          (func $payload (param exnref) (result i32)
            (block $h (result i32)
              (try_table (catch $e $h) (throw_ref (local.get 0)))
              (unreachable)))
          (func $churn (export "churn") (param $n i32)
            (loop $l
              (drop (call $catch (local.get $n)))
              (br_if $l (local.tee $n (i32.sub (local.get $n) (i32.const 1))))))
          (func (export "keep") (param i32) (global.set $g (call $catch (local.get 0))))
          (func (export "recatch") (result exnref)
            (block $h (result exnref)
              (try_table (catch_all_ref $h) (throw_ref (global.get $g)))
              (unreachable)))
          (func (export "recatch_legacy") (result exnref)
            block $h (result exnref)
              try_table (catch_all_ref $h)
                try
                  global.get $g
                  throw_ref
                catch_all
                  rethrow 0
                end
              end
              unreachable
            end)
          (func (export "nested") (param i32) (result i32)
            (local $x exnref)
            (local.set $x (call $catch (local.get 0)))
            ;; No collection runs in the nested execution, and the next ones in this
            ;; execution find `$x` on the stack.
            (call $host (i32.const 2000))
            (call $churn (i32.const 2000))
            (call $payload (local.get $x))))
    "#;

    #[test]
    fn store_reclaims_unreachable_exceptions() {
        use crate::Val;
        let (mut store, instance) = instantiate(MODULE);
        let churn = instance.get_func(&store, "churn").unwrap();
        churn
            .call(&mut store, &[Val::I32(100_000)], &mut [])
            .unwrap();
        let live = store.inner.exns().live();
        assert!(
            live <= 2 * MIN_COLLECTION_THRESHOLD,
            "live exceptions: {live}"
        );
        let nested = instance.get_func(&store, "nested").unwrap();
        let mut result = [Val::I32(0)];
        nested
            .call(&mut store, &[Val::I32(77)], &mut result)
            .unwrap();
        assert_eq!(result[0].i32(), Some(77));
        let live = store.inner.exns().live();
        assert!(
            live <= 4 * MIN_COLLECTION_THRESHOLD,
            "live exceptions: {live}"
        );
    }

    #[test]
    fn rethrown_exceptions_keep_their_address() {
        use crate::Val;
        let (mut store, instance) = instantiate(MODULE);
        let keep = instance.get_func(&store, "keep").unwrap();
        keep.call(&mut store, &[Val::I32(1)], &mut []).unwrap();
        let global = instance.get_global(&store, "g").unwrap().get(&store);
        let Val::ExnRef(Nullable::Val(kept)) = global else {
            panic!("expected an exception reference: {global:?}")
        };
        for name in ["recatch", "recatch_legacy"] {
            let func = instance.get_func(&store, name).unwrap();
            let mut result = [Val::ExnRef(Nullable::Null)];
            func.call(&mut store, &[], &mut result).unwrap();
            let Val::ExnRef(Nullable::Val(caught)) = &result[0] else {
                panic!("expected an exception reference: {:?}", result[0])
            };
            assert_eq!(caught.unwrap_raw(&store), kept.unwrap_raw(&store), "{name}");
        }
    }

    #[test]
    fn stored_exception_fields_are_function_roots() {
        use crate::{Engine, Linker, Module, Store};
        let wat = r#"
            (module
              (tag $f (param funcref))
              (global $g (mut exnref) (ref.null exn))
              (elem declare func $target)
              (func $target (export "target"))
              (func (export "keep")
                (global.set $g
                  (block $h (result exnref)
                    (try_table (catch_all_ref $h) (throw $f (ref.func $target)))
                    (unreachable)))))
        "#;
        let engine = Engine::default();
        let mut store = Store::new(&engine, ());
        let module = Module::new(&engine, wat::parse_str(wat).unwrap()).unwrap();
        let instance = Linker::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .unwrap();
        let target = instance.get_func(&store, "target").unwrap();
        let roots = |store: &Store<()>| {
            let mut roots = alloc::vec::Vec::new();
            store.visit_function_references(|_, _| {}, |function| roots.push(function));
            roots
        };
        assert!(!roots(&store).contains(&target));
        let keep = instance.get_func(&store, "keep").unwrap();
        keep.call(&mut store, &[], &mut []).unwrap();
        assert!(roots(&store).contains(&target));
    }

    #[test]
    fn collections_bound_the_live_exceptions() {
        let mut store = ExnStore::default();
        let mut peak = 0;
        for _ in 0..100_000 {
            if store.wants_collection() {
                store.collect(|_| {});
            }
            store.alloc(exn(&[1, 2, 3])).unwrap();
            peak = peak.max(store.slots.len());
        }
        assert!(peak <= 2 * MIN_COLLECTION_THRESHOLD, "peak slots: {peak}");
    }
}
