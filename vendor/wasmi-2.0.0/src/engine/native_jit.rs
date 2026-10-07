//! TRust: bounded native acceleration of validated Wasmi operator regions.
//!
//! WebAssembly Core, execution/numerics and execution/instructions (local
//! official snapshot 37d6b059, 2026-09-06): integer operations wrap at their
//! declared width; global accesses observe the shared store in instruction
//! order. Calls, traps, fuel, and unsupported operators remain interpreter
//! boundaries. Native code never keeps store pointers across such a boundary.
//! With the `simd` feature, v128 operators are lowered in `simd.rs`.
//!
//! A region is a run of consecutive encoded Wasmi operators of one function,
//! beginning at a control-flow entry (a branch target, a callee entry or a
//! return address). Branches within the region become native jumps; every
//! other control transfer leaves the region with the address of the next
//! operator for the interpreter. Frame cells and the accumulator registers
//! live in Cranelift variables and are written back at every exit.

mod decode;
#[cfg(feature = "simd")]
mod simd;
#[cfg(all(test, feature = "simd"))]
mod spec;
#[cfg(test)]
mod tests;

use self::decode::decode_at;
use crate::{engine::code_map::CodeMap, ir::GlobalAddr};
use alloc::{string::String, vec::Vec};
#[cfg(feature = "simd")]
use cranelift_codegen::ir::Endianness;
use cranelift_codegen::{
    ir::{
        AbiParam,
        Block,
        Function,
        InstBuilder,
        MemFlagsData as MemFlags,
        Type,
        UserFuncName,
        Value,
        condcodes::IntCC,
        types,
    },
    isa::TargetFrontendConfig,
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, default_libcall_names};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    sync::{
        Arc,
        Mutex,
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

const MAX_REGIONS: usize = 128;
pub(crate) const MAX_CANDIDATES: usize = 2048;
/// The maximum number of Wasmi operators in a region.
const MAX_OPERATORS: usize = 256;
/// Shorter regions rarely repay their entry and exit costs.
const MIN_OPERATORS: usize = 4;
pub(crate) const MAX_GLOBALS: usize = 16;
const BACKEDGE_BUDGET: i64 = 16_384;

#[cfg(test)]
std::thread_local! {
    /// Native region invocations on this thread, for tests.
    pub(crate) static REGION_RUNS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// The engine-wide native compiler state: compiled regions and their hotness.
#[derive(Debug)]
pub(crate) struct NativeJit {
    regions: Mutex<HashMap<usize, Candidate>>,
    /// The compiled regions, published once [`MAX_REGIONS`] exist. No further region can be
    /// compiled then, so control-flow entries need neither hotness nor the locked map.
    final_regions: OnceLock<FinalRegions>,
    trace: bool,
    /// Diagnostic stress mode (`WASMI_JIT_EAGER`): compile every control-flow
    /// entry at its first visit, including single-operator regions, without
    /// the region and candidate caps, so that conformance suites whose
    /// functions run once still execute natively.
    eager: AtomicBool,
    /// `WASMI_JIT_TRACE` histogram of the Wasmi operators that ended a region
    /// or declined it, so unsupported hot operators can be found.
    stops: Mutex<BTreeMap<String, (usize, usize)>>,
}

#[derive(Debug, Default)]
struct Candidate {
    visits: u8,
    attempted: bool,
    region: Option<Arc<NativeRegion>>,
}

impl NativeJit {
    pub fn new() -> Self {
        Self {
            regions: Mutex::new(HashMap::new()),
            final_regions: OnceLock::new(),
            trace: std::env::var_os("WASMI_JIT_TRACE").is_some(),
            eager: AtomicBool::new(std::env::var_os("WASMI_JIT_EAGER").is_some()),
            stops: Mutex::new(BTreeMap::new()),
        }
    }

    fn eager(&self) -> bool {
        self.eager.load(Ordering::Relaxed)
    }

    /// Diagnostic and test hook: see [`NativeJit::eager`].
    #[cfg(all(test, feature = "simd"))]
    pub(crate) fn set_eager(&self, eager: bool) {
        self.eager.store(eager, Ordering::Relaxed);
    }

    /// Observe one execution's first visit. Hotness belongs to the Engine, just like
    /// compiled code; short exported calls must not lose it at a JS/host boundary.
    /// None means still warming; Some(None) is a checked interpreter fallback.
    pub fn observe(&self, code_map: &CodeMap, address: usize) -> Option<Option<Arc<NativeRegion>>> {
        let mut regions = self.regions.lock().ok()?;
        if !regions.contains_key(&address) && !self.make_room(&mut regions) {
            return Some(None);
        }
        let candidate = regions.entry(address).or_default();
        if candidate.attempted {
            return Some(candidate.region.clone());
        }
        candidate.visits = candidate.visits.saturating_add(1);
        if candidate.visits <= 32 && !self.eager() {
            return None;
        }
        Some(self.compile_candidate(code_map, address, &mut regions))
    }

    /// Returns the final set of compiled regions once the region cap is reached.
    #[inline]
    pub fn final_regions(&self) -> Option<&FinalRegions> {
        self.final_regions.get()
    }

    pub fn get_or_compile(&self, code_map: &CodeMap, address: usize) -> Option<Arc<NativeRegion>> {
        let mut regions = self.regions.lock().ok()?;
        if !regions.contains_key(&address) && !self.make_room(&mut regions) {
            return None;
        }
        self.compile_candidate(code_map, address, &mut regions)
    }

    /// One-shot startup code must not fill the counter budget and disable later hot functions.
    /// Recycle the coldest warming batch at capacity; completed compilation attempts, including
    /// checked fallbacks, keep their identities. The scan is amortized over cold admissions.
    fn make_room(&self, regions: &mut HashMap<usize, Candidate>) -> bool {
        if regions.len() < MAX_CANDIDATES || self.eager() {
            return true;
        }
        let Some(coldest) = regions
            .values()
            .filter(|candidate| !candidate.attempted)
            .map(|candidate| candidate.visits)
            .min()
        else {
            return false;
        };
        regions.retain(|_, candidate| candidate.attempted || candidate.visits > coldest);
        true
    }

    fn compile_candidate(
        &self,
        code_map: &CodeMap,
        address: usize,
        regions: &mut HashMap<usize, Candidate>,
    ) -> Option<Arc<NativeRegion>> {
        if let Some(candidate) = regions
            .get(&address)
            .filter(|candidate| candidate.attempted)
        {
            return candidate.region.clone();
        }
        let full = !self.eager()
            && regions
                .values()
                .filter(|candidate| candidate.region.is_some())
                .count()
                >= MAX_REGIONS;
        if full {
            // Attempted regions are never evicted, so the compiled set is final.
            self.final_regions.get_or_init(|| FinalRegions::new(regions));
        }
        let candidate = regions.entry(address).or_default();
        candidate.attempted = true;
        if full {
            if self.trace {
                std::eprintln!("[wasmi-jit] declined: {MAX_REGIONS} native regions already exist");
            }
            return None;
        }
        let started = std::time::Instant::now();
        let mut report = Report::default();
        let min_ops = if self.eager() { 1 } else { MIN_OPERATORS };
        let region = code_map
            .compiled_ops_containing(address)
            .and_then(|function| compile_region(function, address, min_ops, &mut report))
            .map(Arc::new);
        candidate.region = region.clone();
        if self.trace {
            self.trace_attempt(region.as_deref(), &report, started.elapsed());
        }
        region
    }

    /// Diagnostic only: called once per compilation attempt with `WASMI_JIT_TRACE`.
    fn trace_attempt(
        &self,
        region: Option<&NativeRegion>,
        report: &Report,
        elapsed: std::time::Duration,
    ) {
        let stop = report.stop.map_or_else(
            || String::from("(end of function)"),
            |code| std::format!("{code:?}"),
        );
        match region {
            Some(region) => std::eprintln!(
                "[wasmi-jit] compiled {} operators at +{}, {} code bytes in {elapsed:?}; stopped before {stop}",
                region.operators,
                region.start - region.function_start,
                region.code_bytes,
            ),
            None => std::eprintln!(
                "[wasmi-jit] declined after {} operators before {stop}: {}",
                report.decoded,
                report.decline.unwrap_or("not compiled"),
            ),
        }
        let Ok(mut stops) = self.stops.lock() else {
            return;
        };
        let entry = stops.entry(stop).or_default();
        entry.0 += 1;
        entry.1 += usize::from(region.is_none());
        let attempts: usize = stops.values().map(|(count, _)| count).sum();
        if attempts % 64 == 0 {
            let mut sorted: Vec<_> = stops.iter().collect();
            sorted.sort_by_key(|(_, (count, _))| core::cmp::Reverse(*count));
            std::eprintln!("[wasmi-jit] region stops after {attempts} attempts (stops, declined):");
            for (name, (count, declined)) in sorted.into_iter().take(24) {
                std::eprintln!("[wasmi-jit]   {name}: {count} ({declined})");
            }
        }
    }

    /// Returns the number of compiled regions.
    #[cfg(test)]
    pub(crate) fn compiled_regions(&self) -> usize {
        self.regions
            .lock()
            .unwrap()
            .values()
            .filter(|candidate| candidate.region.is_some())
            .count()
    }
}

/// The compiled regions after the region cap, with a bit filter that rejects most other
/// control-flow entries before the interpreter leaves its handler.
#[derive(Debug)]
pub(crate) struct FinalRegions {
    regions: rustc_hash::FxHashMap<usize, Arc<NativeRegion>>,
    filter: [u64; FINAL_FILTER_BITS / 64],
}

/// 4096 filter bits keep the false-positive rate near 3% for [`MAX_REGIONS`] regions.
const FINAL_FILTER_BITS: usize = 4096;

impl FinalRegions {
    fn new(candidates: &HashMap<usize, Candidate>) -> Self {
        let mut filter = [0; FINAL_FILTER_BITS / 64];
        let mut regions = rustc_hash::FxHashMap::default();
        for (&address, candidate) in candidates {
            if let Some(region) = &candidate.region {
                let bit = Self::filter_bit(address);
                filter[bit / 64] |= 1 << (bit % 64);
                regions.insert(address, region.clone());
            }
        }
        Self { regions, filter }
    }

    #[inline(always)]
    fn filter_bit(address: usize) -> usize {
        // Fibonacci hashing: the high product bits depend on every address bit.
        (address as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) as usize
            >> (usize::BITS - FINAL_FILTER_BITS.trailing_zeros())
    }

    /// Returns `false` if no region starts at `address`; `true` may be a false positive.
    #[inline(always)]
    pub fn may_contain(&self, address: usize) -> bool {
        let bit = Self::filter_bit(address);
        self.filter[bit / 64] & (1 << (bit % 64)) != 0
    }

    /// Returns the region starting at `address`.
    #[inline]
    pub fn get(&self, address: usize) -> Option<&Arc<NativeRegion>> {
        self.regions.get(&address)
    }
}

/// Where and why region decoding ended, for `WASMI_JIT_TRACE`.
#[derive(Default)]
struct Report {
    decoded: usize,
    stop: Option<crate::ir::OpCode>,
    decline: Option<&'static str>,
}

// JITModule is Send, but not Sync. Its mutex owns the executable allocation;
// executing the immutable finalized code does not access the module. The last
// Arc cannot be dropped while a caller is borrowing the region.
struct CodeOwner(Mutex<Option<JITModule>>);

impl Drop for CodeOwner {
    fn drop(&mut self) {
        let module = self
            .0
            .get_mut()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(module) = module {
            // SAFETY: this owner is private to a region and is dropped only
            // after its last Arc, when no invocation can still be borrowing it.
            unsafe { module.free_memory() };
        }
    }
}

/// The native entry of a region: frame cells, global value addresses, memory
/// context and accumulator registers in, address of the next operator out.
type RegionEntry =
    unsafe extern "C" fn(*mut u64, *const *mut u64, *mut NativeMemory, *mut NativeRegs) -> usize;

pub(crate) struct NativeRegion {
    _owner: CodeOwner,
    entry: RegionEntry,
    pub globals: Vec<GlobalAddr>,
    function_start: usize,
    /// Address of the region's first operator.
    start: usize,
    /// Address following the region's last operator.
    #[cfg_attr(not(all(test, feature = "simd")), allow(dead_code))]
    end: usize,
    operators: usize,
    code_bytes: usize,
}

impl fmt::Debug for NativeRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeRegion")
            .field("globals", &self.globals)
            .finish_non_exhaustive()
    }
}

impl NativeRegion {
    /// Executes the region and returns the address of the next Wasmi operator.
    ///
    /// # Safety
    /// `cells` is the current validated frame, and `globals` contains the live
    /// addresses of this region's global values, in order. Neither may move until
    /// return. No host call or operator that can relocate a store is emitted.
    pub unsafe fn execute(
        &self,
        cells: *mut u64,
        globals: *const *mut u64,
        memory: &mut NativeMemory,
        regs: &mut NativeRegs,
    ) -> usize {
        unsafe { (self.entry)(cells, globals, memory, regs) }
    }
}

/// All pointers are borrowed for one invocation; no native region can grow
/// memory or call the host. Dirty writes are published even when a later
/// access falls back to the interpreter to raise its bounds trap.
#[repr(C)]
pub(crate) struct NativeMemory {
    pub bytes: *mut u8,
    pub len: usize,
    pub dirty_start: usize,
    pub dirty_end: usize,
}

/// The accumulator registers of the interpreter as zero-extended bit patterns.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub(crate) struct NativeRegs {
    pub ireg: u64,
    pub freg32: u64,
    pub freg64: u64,
}

/// An accumulator register of the Wasmi executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reg {
    /// The general purpose register for `i32`, `i64` and reference values.
    I,
    /// The `f32` register.
    F32,
    /// The `f64` register.
    F64,
}

impl Reg {
    const ALL: [Reg; 3] = [Reg::I, Reg::F32, Reg::F64];

    fn index(self) -> usize {
        match self {
            Reg::I => 0,
            Reg::F32 => 1,
            Reg::F64 => 2,
        }
    }

    fn offset(self) -> i32 {
        let offset = match self {
            Reg::I => core::mem::offset_of!(NativeRegs, ireg),
            Reg::F32 => core::mem::offset_of!(NativeRegs, freg32),
            Reg::F64 => core::mem::offset_of!(NativeRegs, freg64),
        };
        offset as i32
    }
}

/// A scalar input of a native operator.
#[derive(Clone, Copy, Debug)]
pub(super) enum Operand {
    /// A frame cell, by index.
    Slot(u32),
    /// An accumulator register.
    Reg(Reg),
    /// An immediate, as the raw bits of its value.
    Constant(i64),
}

/// The destination of a scalar result.
#[derive(Clone, Copy, Debug)]
pub(super) enum Dest {
    /// A frame cell, by index.
    Slot(u32),
    /// An accumulator register.
    Reg(Reg),
    /// Both a frame cell and an accumulator register.
    Both(u32, Reg),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Binary {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    Shl,
    ShrU,
    ShrS,
    Rotl,
    Rotr,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Unary {
    Clz,
    Ctz,
    Popcnt,
    /// Sign-extends the low bits of type `from`.
    Extend(Type),
}

/// A comparison producing an `i32` truth value.
#[derive(Clone, Copy, Debug)]
pub(super) enum Condition {
    Int(IntCC),
    /// Wasmi's fused `(lhs & rhs) != 0`.
    And,
    /// Wasmi's fused `(lhs & rhs) == 0`.
    NotAnd,
    /// Wasmi's fused `(lhs | rhs) != 0`.
    Or,
    /// Wasmi's fused `(lhs | rhs) == 0`.
    NotOr,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Comparison {
    pub ty: Type,
    pub condition: Condition,
    pub lhs: Operand,
    pub rhs: Operand,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum NativeOp {
    /// Copies the low `ty` bits of `value`, zero-extended.
    Copy {
        ty: Type,
        result: Dest,
        value: Operand,
    },
    Unary {
        ty: Type,
        kind: Unary,
        result: Dest,
        value: Operand,
    },
    Binary {
        ty: Type,
        kind: Binary,
        result: Dest,
        lhs: Operand,
        rhs: Operand,
    },
    Compare {
        result: Dest,
        comparison: Comparison,
    },
    /// Branches to the operator at `target` (an absolute address).
    Branch {
        comparison: Option<Comparison>,
        target: usize,
    },
    /// Selects the low `ty` bits of `values[0]` if `condition` is nonzero.
    Select {
        ty: Type,
        result: Dest,
        condition: Operand,
        values: [Operand; 2],
    },
    Load {
        result: Dest,
        ptr: Operand,
        offset: u32,
        width: Type,
        ty: Type,
        signed: bool,
    },
    Store {
        value: Operand,
        ptr: Operand,
        offset: u32,
        width: Type,
    },
    /// Reads the 64-bit value bits of a global.
    GlobalGet {
        result: Dest,
        global: GlobalAddr,
    },
    /// Writes the low `ty` bits of `value`, zero-extended, to a global.
    GlobalSet {
        ty: Type,
        value: Operand,
        global: GlobalAddr,
    },
    #[cfg(feature = "simd")]
    Vector(simd::VectorOp),
}

/// A decoded operator at its address in a function's encoded operators.
#[derive(Clone, Copy, Debug)]
struct RegionOp {
    address: usize,
    next: usize,
    op: NativeOp,
}

/// A validated region: its native operators and the distinct globals they access.
struct Decoded {
    ops: Vec<RegionOp>,
    globals: Vec<GlobalAddr>,
}

fn decode_region(
    function: &[u8],
    start: usize,
    min_ops: usize,
    report: &mut Report,
) -> Option<Decoded> {
    let function_start = function.as_ptr() as usize;
    let function_end = function_start + function.len();
    if !(function_start..function_end).contains(&start) {
        report.decline = Some("entry outside the function");
        return None;
    }
    let mut ops = Vec::new();
    let mut address = start;
    while address < function_end && ops.len() < MAX_OPERATORS {
        let bytes = &function[address - function_start..];
        let (op, next) = match decode_at(bytes, address) {
            Ok(decoded) => decoded,
            Err(stop) => {
                report.stop = stop;
                break;
            }
        };
        ops.push(RegionOp { address, next, op });
        address = next;
    }
    report.decoded = ops.len();
    if ops.len() < min_ops {
        report.decline = Some("too few supported operators");
        return None;
    }
    let mut globals = Vec::new();
    for op in &ops {
        match op.op {
            NativeOp::GlobalGet { global, .. } | NativeOp::GlobalSet { global, .. } => {
                if !globals.contains(&global) {
                    globals.push(global);
                }
            }
            #[cfg(feature = "simd")]
            NativeOp::Vector(vector) => {
                if let Some(global) = vector.global() {
                    if !globals.contains(&global) {
                        globals.push(global);
                    }
                }
            }
            _ => (),
        }
    }
    if globals.len() > MAX_GLOBALS {
        report.decline = Some("too many globals");
        return None;
    }
    let addresses: BTreeSet<usize> = ops.iter().map(|op| op.address).collect();
    let region_end = ops.last().map_or(start, |op| op.next);
    for op in &ops {
        if let NativeOp::Branch { target, .. } = op.op {
            if !(function_start..function_end).contains(&target) {
                report.decline = Some("branch outside the function");
                return None;
            }
            if (start..region_end).contains(&target) && !addresses.contains(&target) {
                report.decline = Some("branch into an operator");
                return None;
            }
        }
    }
    // Falling through the final operator must also remain in the function.
    if region_end >= function_end {
        report.decline = Some("falls through the end of the function");
        return None;
    }
    Some(Decoded { ops, globals })
}

/// Compiles, without running, the regions starting at the function entry and
/// at every branch target of `function` for the named target ISA and preset,
/// checking that each lowering exists there.
#[cfg(all(test, feature = "simd"))]
fn lower_for(isa: &str, preset: &str, function: &[u8]) -> Result<usize, String> {
    use alloc::format;
    use cranelift_codegen::{Context, control::ControlPlane, ir::Signature};
    let mut flags = settings::builder();
    flags
        .set("opt_level", "speed")
        .map_err(|e| format!("{e}"))?;
    let mut builder = cranelift_codegen::isa::lookup_by_name(isa).map_err(|e| format!("{e}"))?;
    if !preset.is_empty() {
        builder.enable(preset).map_err(|e| format!("{e}"))?;
    }
    let isa = builder
        .finish(settings::Flags::new(flags))
        .map_err(|e| format!("{e}"))?;
    let function_start = function.as_ptr() as usize;
    let mut starts = BTreeSet::from([function_start]);
    let mut address = function_start;
    while address < function_start + function.len() {
        let Ok((op, next)) = decode_at(&function[address - function_start..], address) else {
            address = match decode::op_len_at(&function[address - function_start..]) {
                Some(len) => address + len,
                None => break,
            };
            continue;
        };
        if let NativeOp::Branch { target, .. } = op {
            starts.insert(target);
        }
        address = next;
    }
    let mut compiled = 0;
    for start in starts {
        let Some(Decoded { ops, globals }) =
            decode_region(function, start, 1, &mut Report::default())
        else {
            continue;
        };
        let mut ctx = Context::new();
        ctx.func.signature = Signature::new(isa.default_call_conv());
        region_signature(&mut ctx.func.signature);
        build_region(&mut ctx.func, isa.frontend_config(), &ops, &globals);
        ctx.compile(&*isa, &mut ControlPlane::default())
            .map_err(|error| format!("{} region at {start:#x}: {error:?}", isa.name()))?;
        compiled += 1;
    }
    Ok(compiled)
}

/// The region signature: frame cells, global addresses, memory context and
/// registers in, address of the next Wasmi operator out.
fn region_signature(signature: &mut cranelift_codegen::ir::Signature) {
    signature.params.extend([
        AbiParam::new(types::I64),
        AbiParam::new(types::I64),
        AbiParam::new(types::I64),
        AbiParam::new(types::I64),
    ]);
    signature.returns.push(AbiParam::new(types::I64));
}

fn compile_region(
    function: &[u8],
    start: usize,
    min_ops: usize,
    report: &mut Report,
) -> Option<NativeRegion> {
    let Decoded { ops, globals } = decode_region(function, start, min_ops, report)?;
    let mut flags = settings::builder();
    flags.set("use_colocated_libcalls", "false").ok()?;
    flags.set("is_pic", "false").ok()?;
    flags.set("opt_level", "speed").ok()?;
    let isa = cranelift_native::builder()
        .ok()?
        .finish(settings::Flags::new(flags))
        .ok()?;
    let mut module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
    let mut ctx = module.make_context();
    let mut signature = module.make_signature();
    region_signature(&mut signature);
    let id = module
        .declare_function("wasmi_region", Linkage::Local, &signature)
        .ok()?;
    ctx.func.signature = signature;
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    build_region(&mut ctx.func, module.target_config(), &ops, &globals);
    // `WASMI_JIT_DUMP` prints each region's CLIF and lowered machine code.
    let dump = std::env::var_os("WASMI_JIT_DUMP").is_some();
    if dump {
        std::eprintln!("[wasmi-jit] region at {start:#x}:\n{}", ctx.func.display());
        ctx.set_disasm(true);
    }
    // Always free allocations on an unsuccessful compilation as well.
    let finalized = match module.define_function(id, &mut ctx) {
        Ok(()) => module.finalize_definitions(),
        Err(error) => Err(error),
    };
    if let Err(error) = finalized {
        if std::env::var_os("WASMI_JIT_TRACE").is_some() {
            std::eprintln!("[wasmi-jit] compilation fallback: {error}");
        }
        report.decline = Some("Cranelift compilation failed");
        unsafe { module.free_memory() };
        return None;
    }
    if dump {
        if let Some(vcode) = ctx.compiled_code().and_then(|code| code.vcode.as_ref()) {
            std::eprintln!("[wasmi-jit] machine code:\n{vcode}");
        }
    }
    let code = module.get_finalized_function(id);
    let code_bytes = ctx
        .compiled_code()
        .map_or(0, |code| code.code_buffer().len());
    // SAFETY: the generated signature exactly matches this C ABI. Its owner is
    // retained in the returned region until no invocation can remain live.
    let entry = unsafe { core::mem::transmute::<*const u8, RegionEntry>(code) };
    Some(NativeRegion {
        _owner: CodeOwner(Mutex::new(Some(module))),
        entry,
        globals,
        function_start: function.as_ptr() as usize,
        start,
        end: ops.last().map_or(start, |op| op.next),
        operators: ops.len(),
        code_bytes,
    })
}

/// Emits the CLIF body of a decoded region into `func`, whose signature is
/// `(cells, globals, memory, regs) -> address of the next Wasmi operator`.
fn build_region(
    func: &mut Function,
    frontend: TargetFrontendConfig,
    ops: &[RegionOp],
    globals: &[GlobalAddr],
) {
    let mut function_ctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(func, &mut function_ctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let cells_ptr = b.block_params(entry)[0];
    let global_array = b.block_params(entry)[1];
    let regs_ptr = b.block_params(entry)[3];
    let slots = Slots::new(&mut b, cells_ptr, regs_ptr, ops);
    let memory = MemoryCode::new(&mut b, entry);
    let global_ptrs: Vec<_> = (0..globals.len())
        .map(|i| {
            b.ins().load(
                types::I64,
                MemFlags::trusted(),
                global_array,
                (i * 8) as i32,
            )
        })
        .collect();
    let global_ptr = |global: GlobalAddr| global_ptrs[globals.iter().position(|g| *g == global).unwrap()];
    let budget = b.declare_var(types::I32);
    let limit = b.ins().iconst(types::I32, BACKEDGE_BUDGET);
    b.def_var(budget, limit);
    let blocks: Vec<_> = (0..ops.len()).map(|_| b.create_block()).collect();
    let indices: BTreeMap<usize, usize> = ops
        .iter()
        .enumerate()
        .map(|(index, op)| (op.address, index))
        .collect();
    let mut exits = BTreeMap::new();
    b.ins().jump(blocks[0], &[]);
    for (i, region_op) in ops.iter().enumerate() {
        b.switch_to_block(blocks[i]);
        let here = region_op.address;
        match region_op.op {
            NativeOp::Copy { ty, result, value } => {
                let value = slots.read(&mut b, ty, value);
                slots.write(&mut b, result, value);
            }
            NativeOp::Unary {
                ty,
                kind,
                result,
                value,
            } => {
                let value = match kind {
                    Unary::Extend(from) => {
                        let low = slots.read(&mut b, from, value);
                        b.ins().sextend(ty, low)
                    }
                    Unary::Clz => {
                        let value = slots.read(&mut b, ty, value);
                        b.ins().clz(value)
                    }
                    Unary::Ctz => {
                        let value = slots.read(&mut b, ty, value);
                        b.ins().ctz(value)
                    }
                    Unary::Popcnt => {
                        let value = slots.read(&mut b, ty, value);
                        b.ins().popcnt(value)
                    }
                };
                slots.write(&mut b, result, value);
            }
            NativeOp::Binary {
                ty,
                kind,
                result,
                lhs,
                rhs,
            } => {
                let lhs = slots.read(&mut b, ty, lhs);
                let rhs = slots.read(&mut b, ty, rhs);
                let value = match kind {
                    Binary::Add => b.ins().iadd(lhs, rhs),
                    Binary::Sub => b.ins().isub(lhs, rhs),
                    Binary::Mul => b.ins().imul(lhs, rhs),
                    Binary::And => b.ins().band(lhs, rhs),
                    Binary::Or => b.ins().bor(lhs, rhs),
                    Binary::Xor => b.ins().bxor(lhs, rhs),
                    // Cranelift masks shift amounts by the type width, as
                    // Core #op-ishl/#op-ishr_u/#op-ishr_s/#op-irotl/#op-irotr do.
                    Binary::Shl => b.ins().ishl(lhs, rhs),
                    Binary::ShrU => b.ins().ushr(lhs, rhs),
                    Binary::ShrS => b.ins().sshr(lhs, rhs),
                    Binary::Rotl => b.ins().rotl(lhs, rhs),
                    Binary::Rotr => b.ins().rotr(lhs, rhs),
                };
                slots.write(&mut b, result, value);
            }
            NativeOp::Compare { result, comparison } => {
                let value = compare(&mut b, &slots, comparison);
                slots.write(&mut b, result, value);
            }
            NativeOp::Select {
                ty,
                result,
                condition,
                values,
            } => {
                let condition = slots.read(&mut b, types::I32, condition);
                let yes = slots.read(&mut b, ty, values[0]);
                let no = slots.read(&mut b, ty, values[1]);
                let value = b.ins().select(condition, yes, no);
                slots.write(&mut b, result, value);
            }
            NativeOp::GlobalGet { result, global } => {
                let value = b
                    .ins()
                    .load(types::I64, MemFlags::trusted(), global_ptr(global), 0);
                slots.write(&mut b, result, value);
            }
            NativeOp::GlobalSet { ty, value, global } => {
                let value = slots.read(&mut b, ty, value);
                let value = widen_to_word(&mut b, value);
                b.ins()
                    .store(MemFlags::trusted(), value, global_ptr(global), 0);
            }
            #[cfg(feature = "simd")]
            NativeOp::Vector(vector) => {
                let global = vector.global().map(global_ptr);
                simd::emit(&mut b, &slots, &memory, &mut exits, here, global, vector);
            }
            NativeOp::Load {
                result,
                ptr,
                offset,
                width,
                ty,
                signed,
            } => {
                let (address, _) = memory.checked_address(
                    &mut b,
                    &slots,
                    &mut exits,
                    here,
                    MemoryAccess { ptr, offset, width },
                );
                let value = b
                    .ins()
                    .load(width, MemFlags::new().with_notrap(), address, 0);
                let value = if ty == width {
                    value
                } else if signed {
                    b.ins().sextend(ty, value)
                } else {
                    b.ins().uextend(ty, value)
                };
                slots.write(&mut b, result, value);
            }
            NativeOp::Store {
                value,
                ptr,
                offset,
                width,
            } => {
                let (address, relative) = memory.checked_address(
                    &mut b,
                    &slots,
                    &mut exits,
                    here,
                    MemoryAccess { ptr, offset, width },
                );
                let value = slots.read(&mut b, width, value);
                b.ins()
                    .store(MemFlags::new().with_notrap(), value, address, 0);
                memory.mark_dirty(&mut b, relative, width);
            }
            NativeOp::Branch { comparison, target } => {
                let to = edge(&mut b, &blocks, &indices, &mut exits, i, target, budget);
                if let Some(comparison) = comparison {
                    let condition = compare(&mut b, &slots, comparison);
                    let next = destination(&mut b, &blocks, &indices, &mut exits, region_op.next);
                    b.ins().brif(condition, to, &[], next, &[]);
                } else {
                    b.ins().jump(to, &[]);
                }
                continue;
            }
        }
        let next = destination(&mut b, &blocks, &indices, &mut exits, region_op.next);
        b.ins().jump(next, &[]);
    }
    for (target, block) in exits {
        b.switch_to_block(block);
        slots.flush(&mut b);
        memory.flush(&mut b);
        let target = b.ins().iconst(types::I64, target as i64);
        b.ins().return_(&[target]);
    }
    b.seal_all_blocks();
    b.finalize(frontend);
}

/// Returns the immediate `value` as a constant of `ty`, truncated to its width.
fn constant(b: &mut FunctionBuilder<'_>, ty: Type, value: i64) -> Value {
    let value = match ty.bits() {
        8 => i64::from(value as u8),
        16 => i64::from(value as u16),
        32 => i64::from(value as u32),
        _ => value,
    };
    b.ins().iconst(ty, value)
}

/// Zero-extends a scalar to a 64-bit cell word, as Wasmi's cells and registers hold them.
fn widen_to_word(b: &mut FunctionBuilder<'_>, value: Value) -> Value {
    if b.func.dfg.value_type(value) == types::I64 {
        value
    } else {
        b.ins().uextend(types::I64, value)
    }
}

/// Reinterprets a vector value as another 128-bit type. Lane numbering is
/// little-endian, matching both the cell layout of a v128 and Wasm.
#[cfg(feature = "simd")]
fn cast(b: &mut FunctionBuilder<'_>, ty: Type, value: Value) -> Value {
    if b.func.dfg.value_type(value) == ty {
        return value;
    }
    let little = MemFlags::new().with_endianness(Endianness::Little);
    b.ins().bitcast(ty, little, value)
}

/// Frame cells and globals are 8-byte aligned. Whole-v128 accesses therefore
/// omit Cranelift's `aligned` flag, which would permit x86-64 to fold them
/// into 16-byte aligned SSE memory operands.
#[cfg(feature = "simd")]
fn unaligned() -> MemFlags {
    MemFlags::new().with_notrap()
}

/// How one frame cell is held in Cranelift variables within a region.
#[derive(Clone, Copy)]
enum CellVar {
    /// The 64-bit cell as an integer.
    Word(Variable),
    /// The low (`false`) or high (`true`) half of a v128 held as one `i8x16`.
    /// Every v128 access of a non-overlapping cell pair uses this form, so
    /// vector values stay in vector registers; scalar accesses to such a
    /// cell use its 64-bit lane.
    #[cfg(feature = "simd")]
    Vector { variable: Variable, high: bool },
}

struct Slots {
    cells_ptr: Value,
    regs_ptr: Value,
    cells: BTreeMap<u32, CellVar>,
    /// The v128 cell pairs held in vector variables, by their first cell.
    #[cfg(feature = "simd")]
    vectors: BTreeMap<u32, Variable>,
    dirty: BTreeSet<u32>,
    regs: [Variable; 3],
    dirty_regs: [bool; 3],
}

impl Slots {
    fn new(b: &mut FunctionBuilder<'_>, cells_ptr: Value, regs_ptr: Value, ops: &[RegionOp]) -> Self {
        let mut all = BTreeSet::new();
        let mut dirty = BTreeSet::new();
        let mut vector_starts = BTreeSet::new();
        let mut dirty_regs = [false; 3];
        for op in ops {
            op.op.visit_cells(&mut |cell, written, vector| {
                all.insert(cell);
                if written {
                    dirty.insert(cell);
                }
                if vector {
                    all.insert(cell + 1);
                    if written {
                        dirty.insert(cell + 1);
                    }
                    vector_starts.insert(cell);
                }
            });
            op.op.visit_regs(&mut |reg, written| {
                if written {
                    dirty_regs[reg.index()] = true;
                }
            });
        }
        #[cfg(not(feature = "simd"))]
        let _ = vector_starts;
        let regs = Reg::ALL.map(|reg| {
            let variable = b.declare_var(types::I64);
            let value = b
                .ins()
                .load(types::I64, MemFlags::trusted(), regs_ptr, reg.offset());
            b.def_var(variable, value);
            variable
        });
        let mut cells = BTreeMap::new();
        #[cfg(feature = "simd")]
        let mut vectors = BTreeMap::new();
        #[cfg(feature = "simd")]
        for &start in &vector_starts {
            // Overlapping pairs cannot share one vector variable; they use words.
            let overlaps = start > 0 && vector_starts.contains(&(start - 1))
                || vector_starts.contains(&(start + 1));
            if overlaps {
                continue;
            }
            let variable = b.declare_var(types::I8X16);
            let value = b
                .ins()
                .load(types::I8X16, unaligned(), cells_ptr, cell_offset(start));
            b.def_var(variable, value);
            vectors.insert(start, variable);
            cells.insert(start, CellVar::Vector { variable, high: false });
            cells.insert(start + 1, CellVar::Vector { variable, high: true });
        }
        for cell in all {
            if cells.contains_key(&cell) {
                continue;
            }
            let variable = b.declare_var(types::I64);
            let value = b
                .ins()
                .load(types::I64, MemFlags::trusted(), cells_ptr, cell_offset(cell));
            b.def_var(variable, value);
            cells.insert(cell, CellVar::Word(variable));
        }
        Self {
            cells_ptr,
            regs_ptr,
            cells,
            #[cfg(feature = "simd")]
            vectors,
            dirty,
            regs,
            dirty_regs,
        }
    }

    /// The complete 64-bit word of a cell.
    fn word(&self, b: &mut FunctionBuilder<'_>, cell: u32) -> Value {
        match self.cells[&cell] {
            CellVar::Word(variable) => b.use_var(variable),
            #[cfg(feature = "simd")]
            CellVar::Vector { variable, high } => {
                let value = b.use_var(variable);
                let value = cast(b, types::I64X2, value);
                b.ins().extractlane(value, u8::from(high))
            }
        }
    }

    /// Replaces the complete 64-bit word of a cell.
    fn set_word(&self, b: &mut FunctionBuilder<'_>, cell: u32, value: Value) {
        match self.cells[&cell] {
            CellVar::Word(variable) => b.def_var(variable, value),
            #[cfg(feature = "simd")]
            CellVar::Vector { variable, high } => {
                let old = b.use_var(variable);
                let old = cast(b, types::I64X2, old);
                let new = b.ins().insertlane(old, value, u8::from(high));
                let new = cast(b, types::I8X16, new);
                b.def_var(variable, new);
            }
        }
    }

    /// Reads the low `ty` bits of an operand as a scalar.
    fn read(&self, b: &mut FunctionBuilder<'_>, ty: Type, operand: Operand) -> Value {
        let word = match operand {
            Operand::Slot(cell) => self.word(b, cell),
            Operand::Reg(reg) => b.use_var(self.regs[reg.index()]),
            Operand::Constant(value) => return constant(b, ty, value),
        };
        if ty == types::I64 {
            word
        } else {
            b.ins().ireduce(ty, word)
        }
    }

    /// Writes a scalar zero-extended to 64 bits, as Wasmi's cells and
    /// accumulator registers hold `i32` and `f32` values.
    fn write(&self, b: &mut FunctionBuilder<'_>, result: Dest, value: Value) {
        let value = widen_to_word(b, value);
        match result {
            Dest::Slot(cell) => self.set_word(b, cell, value),
            Dest::Reg(reg) => b.def_var(self.regs[reg.index()], value),
            Dest::Both(cell, reg) => {
                self.set_word(b, cell, value);
                b.def_var(self.regs[reg.index()], value);
            }
        }
    }

    /// The v128 held in cells `cell` and `cell + 1` as a vector of type `ty`.
    #[cfg(feature = "simd")]
    fn vector(&self, b: &mut FunctionBuilder<'_>, cell: u32, ty: Type) -> Value {
        let value = match self.vectors.get(&cell) {
            Some(variable) => b.use_var(*variable),
            None => {
                let low = self.word(b, cell);
                let high = self.word(b, cell + 1);
                let value = b.ins().scalar_to_vector(types::I64X2, low);
                b.ins().insertlane(value, high, 1)
            }
        };
        cast(b, ty, value)
    }

    /// Replaces the v128 held in cells `cell` and `cell + 1`.
    #[cfg(feature = "simd")]
    fn set_vector(&self, b: &mut FunctionBuilder<'_>, cell: u32, value: Value) {
        match self.vectors.get(&cell) {
            Some(variable) => {
                let value = cast(b, types::I8X16, value);
                b.def_var(*variable, value);
            }
            None => {
                let value = cast(b, types::I64X2, value);
                for lane in 0..2 {
                    let word = b.ins().extractlane(value, lane);
                    self.set_word(b, cell + u32::from(lane), word);
                }
            }
        }
    }

    fn flush(&self, b: &mut FunctionBuilder<'_>) {
        #[cfg(feature = "simd")]
        let mut stored_vectors = BTreeSet::new();
        for &cell in &self.dirty {
            match self.cells[&cell] {
                CellVar::Word(variable) => {
                    let value = b.use_var(variable);
                    b.ins()
                        .store(MemFlags::trusted(), value, self.cells_ptr, cell_offset(cell));
                }
                #[cfg(feature = "simd")]
                CellVar::Vector { variable, high } => {
                    let start = cell - u32::from(high);
                    if stored_vectors.insert(start) {
                        let value = b.use_var(variable);
                        b.ins()
                            .store(unaligned(), value, self.cells_ptr, cell_offset(start));
                    }
                }
            }
        }
        for reg in Reg::ALL {
            if self.dirty_regs[reg.index()] {
                let value = b.use_var(self.regs[reg.index()]);
                b.ins()
                    .store(MemFlags::trusted(), value, self.regs_ptr, reg.offset());
            }
        }
    }
}

fn cell_offset(cell: u32) -> i32 {
    (cell * 8) as i32
}

impl Operand {
    fn visit(self, cells: &mut dyn FnMut(u32, bool, bool), regs: &mut dyn FnMut(Reg, bool)) {
        match self {
            Operand::Slot(cell) => cells(cell, false, false),
            Operand::Reg(reg) => regs(reg, false),
            Operand::Constant(_) => {}
        }
    }
}

impl Dest {
    fn visit(self, cells: &mut dyn FnMut(u32, bool, bool), regs: &mut dyn FnMut(Reg, bool)) {
        match self {
            Dest::Slot(cell) => cells(cell, true, false),
            Dest::Reg(reg) => regs(reg, true),
            Dest::Both(cell, reg) => {
                cells(cell, true, false);
                regs(reg, true);
            }
        }
    }
}

impl NativeOp {
    /// Visits every operand and destination: cells and registers, the former as
    /// `(cell, written, holds_v128)` and the latter as `(register, written)`.
    fn visit(&self, cells: &mut dyn FnMut(u32, bool, bool), regs: &mut dyn FnMut(Reg, bool)) {
        use NativeOp as N;
        match *self {
            N::Copy { result, value, .. } | N::Unary { result, value, .. } => {
                value.visit(cells, regs);
                result.visit(cells, regs);
            }
            N::Binary {
                result, lhs, rhs, ..
            } => {
                lhs.visit(cells, regs);
                rhs.visit(cells, regs);
                result.visit(cells, regs);
            }
            N::Compare { result, comparison } => {
                comparison.lhs.visit(cells, regs);
                comparison.rhs.visit(cells, regs);
                result.visit(cells, regs);
            }
            N::Branch { comparison, .. } => {
                if let Some(comparison) = comparison {
                    comparison.lhs.visit(cells, regs);
                    comparison.rhs.visit(cells, regs);
                }
            }
            N::Select {
                result,
                condition,
                values,
                ..
            } => {
                condition.visit(cells, regs);
                values[0].visit(cells, regs);
                values[1].visit(cells, regs);
                result.visit(cells, regs);
            }
            N::Load { result, ptr, .. } => {
                ptr.visit(cells, regs);
                result.visit(cells, regs);
            }
            N::Store { value, ptr, .. } => {
                value.visit(cells, regs);
                ptr.visit(cells, regs);
            }
            N::GlobalGet { result, .. } => result.visit(cells, regs),
            N::GlobalSet { value, .. } => value.visit(cells, regs),
            #[cfg(feature = "simd")]
            N::Vector(vector) => vector.visit(cells, regs),
        }
    }

    fn visit_cells(&self, cells: &mut dyn FnMut(u32, bool, bool)) {
        self.visit(cells, &mut |_, _| {});
    }

    fn visit_regs(&self, regs: &mut dyn FnMut(Reg, bool)) {
        self.visit(&mut |_, _, _| {}, regs);
    }
}

struct MemoryAccess {
    ptr: Operand,
    offset: u32,
    width: Type,
}

struct MemoryCode {
    context: Value,
    bytes: Value,
    len: Value,
    dirty_start: Variable,
    dirty_end: Variable,
}

impl MemoryCode {
    fn new(b: &mut FunctionBuilder<'_>, entry: Block) -> Self {
        let context = b.block_params(entry)[2];
        let bytes = b.ins().load(
            types::I64,
            MemFlags::trusted(),
            context,
            core::mem::offset_of!(NativeMemory, bytes) as i32,
        );
        let len = b.ins().load(
            types::I64,
            MemFlags::trusted(),
            context,
            core::mem::offset_of!(NativeMemory, len) as i32,
        );
        let dirty_start = b.declare_var(types::I64);
        let dirty_end = b.declare_var(types::I64);
        let max = b.ins().iconst(types::I64, -1);
        let zero = b.ins().iconst(types::I64, 0);
        b.def_var(dirty_start, max);
        b.def_var(dirty_end, zero);
        Self {
            context,
            bytes,
            len,
            dirty_start,
            dirty_end,
        }
    }

    /// Returns the host address and the memory-relative address of a checked
    /// access; a failed bounds check leaves the region at `operator`.
    fn checked_address(
        &self,
        b: &mut FunctionBuilder<'_>,
        slots: &Slots,
        exits: &mut BTreeMap<usize, Block>,
        operator: usize,
        access: MemoryAccess,
    ) -> (Value, Value) {
        let MemoryAccess { ptr, offset, width } = access;
        // Core #exec-load / #exec-store: check the mathematical sum, without
        // wrapping the effective address. The subtraction is used only when
        // length >= offset + access width, including for memory64 pointers.
        let ptr = slots.read(b, types::I64, ptr);
        let needed = b
            .ins()
            .iconst(types::I64, i64::from(offset) + i64::from(width.bytes()));
        let enough = b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, self.len, needed);
        let limit = b.ins().isub(self.len, needed);
        let in_range = b.ins().icmp(IntCC::UnsignedLessThanOrEqual, ptr, limit);
        let valid = b.ins().band(enough, in_range);
        let relative = b.ins().iadd_imm_u(ptr, i64::from(offset));
        let address = b.ins().iadd(self.bytes, relative);
        // On a mispredicted bounds branch, restrict speculative memory access
        // to our own context instead of an attacker-selected host address.
        let address = b.ins().select_spectre_guard(valid, address, self.context);
        let access = b.create_block();
        let exit = *exits.entry(operator).or_insert_with(|| b.create_block());
        b.ins().brif(valid, access, &[], exit, &[]);
        b.switch_to_block(access);
        (address, relative)
    }

    fn mark_dirty(&self, b: &mut FunctionBuilder<'_>, address: Value, width: Type) {
        let old_start = b.use_var(self.dirty_start);
        let old_end = b.use_var(self.dirty_end);
        let end = b.ins().iadd_imm_u(address, i64::from(width.bytes()));
        let start = b.ins().umin(old_start, address);
        let end = b.ins().umax(old_end, end);
        b.def_var(self.dirty_start, start);
        b.def_var(self.dirty_end, end);
    }

    fn flush(&self, b: &mut FunctionBuilder<'_>) {
        let start = b.use_var(self.dirty_start);
        let end = b.use_var(self.dirty_end);
        b.ins().store(
            MemFlags::trusted(),
            start,
            self.context,
            core::mem::offset_of!(NativeMemory, dirty_start) as i32,
        );
        b.ins().store(
            MemFlags::trusted(),
            end,
            self.context,
            core::mem::offset_of!(NativeMemory, dirty_end) as i32,
        );
    }
}

/// Evaluates `comparison` to an `i8` truth value.
fn compare(b: &mut FunctionBuilder<'_>, slots: &Slots, comparison: Comparison) -> Value {
    let lhs = slots.read(b, comparison.ty, comparison.lhs);
    let rhs = slots.read(b, comparison.ty, comparison.rhs);
    match comparison.condition {
        Condition::Int(cc) => b.ins().icmp(cc, lhs, rhs),
        Condition::And | Condition::NotAnd => {
            let bits = b.ins().band(lhs, rhs);
            let cc = match comparison.condition {
                Condition::And => IntCC::NotEqual,
                _ => IntCC::Equal,
            };
            b.ins().icmp_imm_u(cc, bits, 0)
        }
        Condition::Or | Condition::NotOr => {
            let bits = b.ins().bor(lhs, rhs);
            let cc = match comparison.condition {
                Condition::Or => IntCC::NotEqual,
                _ => IntCC::Equal,
            };
            b.ins().icmp_imm_u(cc, bits, 0)
        }
    }
}

/// The block executing the operator at `target`, or the exit to it.
fn destination(
    b: &mut FunctionBuilder<'_>,
    blocks: &[Block],
    indices: &BTreeMap<usize, usize>,
    exits: &mut BTreeMap<usize, Block>,
    target: usize,
) -> Block {
    if let Some(&index) = indices.get(&target) {
        return blocks[index];
    }
    *exits.entry(target).or_insert_with(|| b.create_block())
}

/// Like [`destination`], but a backward edge within the region spends the
/// back-edge budget and leaves the region once it is exhausted.
fn edge(
    b: &mut FunctionBuilder<'_>,
    blocks: &[Block],
    indices: &BTreeMap<usize, usize>,
    exits: &mut BTreeMap<usize, Block>,
    current: usize,
    target: usize,
    budget: Variable,
) -> Block {
    let to = destination(b, blocks, indices, exits, target);
    match indices.get(&target) {
        Some(&index) if index <= current => {}
        _ => return to,
    }
    let origin = b.current_block().unwrap();
    let check = b.create_block();
    b.switch_to_block(check);
    let old = b.use_var(budget);
    let next = b.ins().iadd_imm_s(old, -1);
    b.def_var(budget, next);
    let exit = *exits.entry(target).or_insert_with(|| b.create_block());
    b.ins().brif(next, to, &[], exit, &[]);
    b.switch_to_block(origin);
    check
}
