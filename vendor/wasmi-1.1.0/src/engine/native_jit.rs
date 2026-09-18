//! Bounded native acceleration of validated Wasmi instruction regions.
//!
//! WebAssembly Core, execution/numerics and execution/instructions (local
//! official snapshot 37d6b059, 2026-09-06): integer operations wrap at their
//! declared width; global accesses observe the shared store in instruction
//! order. Calls, traps, fuel, and unsupported instructions remain interpreter
//! boundaries. Native code never keeps store pointers across such a boundary.

use super::code_map::CodeMap;
use crate::{
    core::UntypedVal,
    ir::{index, Op, Slot},
};
use alloc::vec::Vec;
use cranelift_codegen::{
    ir::{
        condcodes::IntCC, types, AbiParam, Block, InstBuilder, MemFlagsData as MemFlags, Type,
        UserFuncName, Value,
    },
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    sync::{Arc, Mutex},
};

const MAX_REGIONS: usize = 128;
pub(crate) const MAX_CANDIDATES: usize = 2048;
const MAX_INSTRUCTIONS: usize = 256;
pub(crate) const MAX_GLOBALS: usize = 16;
const BACKEDGE_BUDGET: i64 = 16_384;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(crate) struct NativeJit {
    pub enabled: bool,
    regions: Mutex<HashMap<usize, Option<Arc<NativeRegion>>>>,
    trace: bool,
}

impl NativeJit {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            regions: Mutex::new(HashMap::new()),
            trace: std::env::var_os("WASMI_JIT_TRACE").is_some(),
        }
    }

    pub fn get_or_compile(&self, code_map: &CodeMap, address: usize) -> Option<Arc<NativeRegion>> {
        let mut regions = self.regions.lock().ok()?;
        if let Some(region) = regions.get(&address) {
            return region.clone();
        }
        if regions.len() >= MAX_CANDIDATES
            || regions.values().filter(|r| r.is_some()).count() >= MAX_REGIONS
        {
            return None;
        }
        let started = std::time::Instant::now();
        let region = code_map
            .function_at(address)
            .and_then(|(instrs, start)| compile(instrs, start))
            .map(Arc::new);
        if self.trace {
            if let Some(region) = &region {
                std::eprintln!(
                    "[wasmi-jit] compiled {} instructions, {} code bytes in {:?}",
                    region.instructions,
                    region.code_bytes,
                    started.elapsed()
                );
            }
        }
        regions.insert(address, region.clone());
        region
    }
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

pub(crate) struct NativeRegion {
    _owner: CodeOwner,
    entry:
        unsafe extern "C" fn(*mut UntypedVal, *const *mut UntypedVal, *mut NativeMemory) -> usize,
    pub globals: Vec<index::Global>,
    function_base: usize,
    instructions: usize,
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
    /// # Safety
    /// `slots` is the current validated frame, and `globals` contains the live
    /// addresses of this region's globals, in order. Neither may move until
    /// return. No host call or instruction that can relocate a store is emitted.
    pub unsafe fn execute(
        &self,
        slots: *mut UntypedVal,
        globals: *const *mut UntypedVal,
        memory: &mut NativeMemory,
    ) -> *const Op {
        let next = unsafe { (self.entry)(slots, globals, memory) };
        (self.function_base as *const Op).wrapping_add(next)
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

#[derive(Clone, Copy)]
enum Operand {
    Slot(Slot),
    Constant(i64),
}

#[derive(Clone, Copy)]
enum Binary {
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

#[derive(Clone, Copy)]
struct Comparison {
    ty: Type,
    cc: IntCC,
    lhs: Operand,
    rhs: Operand,
}

#[derive(Clone, Copy)]
enum NativeOp {
    Copy {
        result: Slot,
        value: Slot,
    },
    Constant {
        result: Slot,
        value: i64,
    },
    GlobalGet {
        result: Slot,
        global: index::Global,
    },
    GlobalSet {
        input: Slot,
        global: index::Global,
    },
    GlobalConstant {
        value: i64,
        global: index::Global,
    },
    Binary {
        ty: Type,
        kind: Binary,
        result: Slot,
        lhs: Operand,
        rhs: Operand,
    },
    Compare {
        result: Slot,
        comparison: Comparison,
    },
    Branch {
        comparison: Option<Comparison>,
        offset: i32,
    },
    Load {
        result: Slot,
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
    Convert {
        result: Slot,
        input: Slot,
        from: Type,
        to: Type,
        signed: bool,
    },
    Select {
        result: Slot,
        comparison: Comparison,
        values: [Slot; 2],
    },
    Parameter,
}

fn decode(op: Op) -> Option<NativeOp> {
    use NativeOp as N;
    use Operand::{Constant as C, Slot as S};
    Some(match op {
        Op::Copy { result, value } => N::Copy { result, value },
        Op::CopyImm32 { result, value } => N::Constant {
            result,
            value: u32::from(value).into(),
        },
        Op::CopyI64Imm32 { result, value } => N::Constant {
            result,
            value: i64::from(value),
        },
        Op::GlobalGet { result, global } => N::GlobalGet { result, global },
        Op::GlobalSet { input, global } => N::GlobalSet { input, global },
        Op::GlobalSetI32Imm16 { input, global } => N::GlobalConstant {
            value: i64::from(i32::from(input) as u32),
            global,
        },
        Op::GlobalSetI64Imm16 { input, global } => N::GlobalConstant {
            value: i64::from(input),
            global,
        },
        Op::I32WrapI64 { result, input } => N::Convert {
            result,
            input,
            from: types::I32,
            to: types::I32,
            signed: false,
        },
        Op::I32Extend8S { result, input } => N::Convert {
            result,
            input,
            from: types::I8,
            to: types::I32,
            signed: true,
        },
        Op::I32Extend16S { result, input } => N::Convert {
            result,
            input,
            from: types::I16,
            to: types::I32,
            signed: true,
        },
        Op::I64Extend8S { result, input } => N::Convert {
            result,
            input,
            from: types::I8,
            to: types::I64,
            signed: true,
        },
        Op::I64Extend16S { result, input } => N::Convert {
            result,
            input,
            from: types::I16,
            to: types::I64,
            signed: true,
        },
        Op::I64Extend32S { result, input } => N::Convert {
            result,
            input,
            from: types::I32,
            to: types::I64,
            signed: true,
        },
        Op::BranchI32Eq { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::Equal,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32EqImm16 { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::Equal,
                lhs: S(lhs),
                rhs: C(i64::from(i32::from(rhs))),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32Ne { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::NotEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32NeImm16 { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::NotEqual,
                lhs: S(lhs),
                rhs: C(i64::from(i32::from(rhs))),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LtS { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThan,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LtSImm16Lhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThan,
                lhs: C(i64::from(i32::from(lhs))),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LtSImm16Rhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThan,
                lhs: S(lhs),
                rhs: C(i64::from(i32::from(rhs))),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LtU { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThan,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LtUImm16Lhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThan,
                lhs: C(i64::from(u32::from(lhs))),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LtUImm16Rhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThan,
                lhs: S(lhs),
                rhs: C(i64::from(u32::from(rhs))),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LeS { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LeSImm16Lhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: C(i64::from(i32::from(lhs))),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LeSImm16Rhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: C(i64::from(i32::from(rhs))),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LeU { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LeUImm16Lhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: C(i64::from(u32::from(lhs))),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI32LeUImm16Rhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: C(i64::from(u32::from(rhs))),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64Eq { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::Equal,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64EqImm16 { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::Equal,
                lhs: S(lhs),
                rhs: C(i64::from(rhs)),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64Ne { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::NotEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64NeImm16 { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::NotEqual,
                lhs: S(lhs),
                rhs: C(i64::from(rhs)),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LtS { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThan,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LtSImm16Lhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThan,
                lhs: C(i64::from(lhs)),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LtSImm16Rhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThan,
                lhs: S(lhs),
                rhs: C(i64::from(rhs)),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LtU { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThan,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LtUImm16Lhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThan,
                lhs: C(u64::from(lhs) as i64),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LtUImm16Rhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThan,
                lhs: S(lhs),
                rhs: C(u64::from(rhs) as i64),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LeS { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LeSImm16Lhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: C(i64::from(lhs)),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LeSImm16Rhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: C(i64::from(rhs)),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LeU { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LeUImm16Lhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: C(u64::from(lhs) as i64),
                rhs: S(rhs),
            }),
            offset: offset.to_i16().into(),
        },
        Op::BranchI64LeUImm16Rhs { lhs, rhs, offset } => N::Branch {
            comparison: Some(Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: C(u64::from(rhs) as i64),
            }),
            offset: offset.to_i16().into(),
        },
        Op::I32Eq { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::Equal,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I32EqImm16 { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::Equal,
                lhs: S(lhs),
                rhs: C(i64::from(i32::from(rhs))),
            },
        },
        Op::I32Ne { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::NotEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I32NeImm16 { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::NotEqual,
                lhs: S(lhs),
                rhs: C(i64::from(i32::from(rhs))),
            },
        },
        Op::I32LtS { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThan,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I32LtSImm16Lhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThan,
                lhs: C(i64::from(i32::from(lhs))),
                rhs: S(rhs),
            },
        },
        Op::I32LtSImm16Rhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThan,
                lhs: S(lhs),
                rhs: C(i64::from(i32::from(rhs))),
            },
        },
        Op::I32LtU { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThan,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I32LtUImm16Lhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThan,
                lhs: C(i64::from(u32::from(lhs))),
                rhs: S(rhs),
            },
        },
        Op::I32LtUImm16Rhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThan,
                lhs: S(lhs),
                rhs: C(i64::from(u32::from(rhs))),
            },
        },
        Op::I32LeS { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I32LeSImm16Lhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: C(i64::from(i32::from(lhs))),
                rhs: S(rhs),
            },
        },
        Op::I32LeSImm16Rhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: C(i64::from(i32::from(rhs))),
            },
        },
        Op::I32LeU { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I32LeUImm16Lhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: C(i64::from(u32::from(lhs))),
                rhs: S(rhs),
            },
        },
        Op::I32LeUImm16Rhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: C(i64::from(u32::from(rhs))),
            },
        },
        Op::I64Eq { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::Equal,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I64EqImm16 { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::Equal,
                lhs: S(lhs),
                rhs: C(i64::from(rhs)),
            },
        },
        Op::I64Ne { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::NotEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I64NeImm16 { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::NotEqual,
                lhs: S(lhs),
                rhs: C(i64::from(rhs)),
            },
        },
        Op::I64LtS { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThan,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I64LtSImm16Lhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThan,
                lhs: C(i64::from(lhs)),
                rhs: S(rhs),
            },
        },
        Op::I64LtSImm16Rhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThan,
                lhs: S(lhs),
                rhs: C(i64::from(rhs)),
            },
        },
        Op::I64LtU { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThan,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I64LtUImm16Lhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThan,
                lhs: C(u64::from(lhs) as i64),
                rhs: S(rhs),
            },
        },
        Op::I64LtUImm16Rhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThan,
                lhs: S(lhs),
                rhs: C(u64::from(rhs) as i64),
            },
        },
        Op::I64LeS { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I64LeSImm16Lhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: C(i64::from(lhs)),
                rhs: S(rhs),
            },
        },
        Op::I64LeSImm16Rhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: C(i64::from(rhs)),
            },
        },
        Op::I64LeU { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: S(rhs),
            },
        },
        Op::I64LeUImm16Lhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: C(u64::from(lhs) as i64),
                rhs: S(rhs),
            },
        },
        Op::I64LeUImm16Rhs { result, lhs, rhs } => N::Compare {
            result,
            comparison: Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: S(lhs),
                rhs: C(u64::from(rhs) as i64),
            },
        },
        Op::I32Add { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Add,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32AddImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Add,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32Sub { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Sub,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32SubImm16Lhs { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Sub,
            result,
            lhs: C(i64::from(i32::from(lhs))),
            rhs: S(rhs),
        },
        Op::I32Mul { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Mul,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32MulImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Mul,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32BitAnd { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::And,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32BitAndImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::And,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32BitOr { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Or,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32BitOrImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Or,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32BitXor { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Xor,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32BitXorImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Xor,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32Shl { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Shl,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32ShlBy { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Shl,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32ShlImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Shl,
            result,
            lhs: C(i64::from(i32::from(lhs))),
            rhs: S(rhs),
        },
        Op::I32ShrU { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::ShrU,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32ShrUBy { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::ShrU,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32ShrUImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::ShrU,
            result,
            lhs: C(i64::from(i32::from(lhs))),
            rhs: S(rhs),
        },
        Op::I32ShrS { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::ShrS,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32ShrSBy { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::ShrS,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32ShrSImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::ShrS,
            result,
            lhs: C(i64::from(i32::from(lhs))),
            rhs: S(rhs),
        },
        Op::I32Rotl { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Rotl,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32RotlBy { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Rotl,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32RotlImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Rotl,
            result,
            lhs: C(i64::from(i32::from(lhs))),
            rhs: S(rhs),
        },
        Op::I32Rotr { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Rotr,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I32RotrBy { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Rotr,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(i32::from(rhs))),
        },
        Op::I32RotrImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I32,
            kind: Binary::Rotr,
            result,
            lhs: C(i64::from(i32::from(lhs))),
            rhs: S(rhs),
        },
        Op::I64Add { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Add,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64AddImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Add,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64Sub { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Sub,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64SubImm16Lhs { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Sub,
            result,
            lhs: C(i64::from(lhs)),
            rhs: S(rhs),
        },
        Op::I64Mul { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Mul,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64MulImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Mul,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64BitAnd { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::And,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64BitAndImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::And,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64BitOr { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Or,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64BitOrImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Or,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64BitXor { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Xor,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64BitXorImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Xor,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64Shl { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Shl,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64ShlBy { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Shl,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64ShlImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Shl,
            result,
            lhs: C(i64::from(lhs)),
            rhs: S(rhs),
        },
        Op::I64ShrU { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::ShrU,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64ShrUBy { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::ShrU,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64ShrUImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::ShrU,
            result,
            lhs: C(i64::from(lhs)),
            rhs: S(rhs),
        },
        Op::I64ShrS { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::ShrS,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64ShrSBy { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::ShrS,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64ShrSImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::ShrS,
            result,
            lhs: C(i64::from(lhs)),
            rhs: S(rhs),
        },
        Op::I64Rotl { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Rotl,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64RotlBy { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Rotl,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64RotlImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Rotl,
            result,
            lhs: C(i64::from(lhs)),
            rhs: S(rhs),
        },
        Op::I64Rotr { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Rotr,
            result,
            lhs: S(lhs),
            rhs: S(rhs),
        },
        Op::I64RotrBy { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Rotr,
            result,
            lhs: S(lhs),
            rhs: C(i64::from(rhs)),
        },
        Op::I64RotrImm16 { result, lhs, rhs } => N::Binary {
            ty: types::I64,
            kind: Binary::Rotr,
            result,
            lhs: C(i64::from(lhs)),
            rhs: S(rhs),
        },
        Op::Branch { offset } => N::Branch {
            comparison: None,
            offset: offset.to_i32(),
        },
        Op::Load32At { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I32,
            ty: types::I32,
            signed: false,
        },
        Op::Load32Offset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I32,
            ty: types::I32,
            signed: false,
        },
        Op::Load64At { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I64,
            ty: types::I64,
            signed: false,
        },
        Op::Load64Offset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I64,
            ty: types::I64,
            signed: false,
        },
        Op::I32Load8sAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I8,
            ty: types::I32,
            signed: true,
        },
        Op::I32Load8sOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I8,
            ty: types::I32,
            signed: true,
        },
        Op::I32Load8uAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I8,
            ty: types::I32,
            signed: false,
        },
        Op::I32Load8uOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I8,
            ty: types::I32,
            signed: false,
        },
        Op::I32Load16sAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I16,
            ty: types::I32,
            signed: true,
        },
        Op::I32Load16sOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I16,
            ty: types::I32,
            signed: true,
        },
        Op::I32Load16uAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I16,
            ty: types::I32,
            signed: false,
        },
        Op::I32Load16uOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I16,
            ty: types::I32,
            signed: false,
        },
        Op::I64Load8sAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I8,
            ty: types::I64,
            signed: true,
        },
        Op::I64Load8sOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I8,
            ty: types::I64,
            signed: true,
        },
        Op::I64Load8uAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I8,
            ty: types::I64,
            signed: false,
        },
        Op::I64Load8uOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I8,
            ty: types::I64,
            signed: false,
        },
        Op::I64Load16sAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I16,
            ty: types::I64,
            signed: true,
        },
        Op::I64Load16sOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I16,
            ty: types::I64,
            signed: true,
        },
        Op::I64Load16uAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I16,
            ty: types::I64,
            signed: false,
        },
        Op::I64Load16uOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I16,
            ty: types::I64,
            signed: false,
        },
        Op::I64Load32sAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I32,
            ty: types::I64,
            signed: true,
        },
        Op::I64Load32sOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I32,
            ty: types::I64,
            signed: true,
        },
        Op::I64Load32uAt { result, address } => N::Load {
            result,
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I32,
            ty: types::I64,
            signed: false,
        },
        Op::I64Load32uOffset16 {
            result,
            ptr,
            offset,
        } => N::Load {
            result,
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I32,
            ty: types::I64,
            signed: false,
        },
        Op::Store32Offset16 { value, ptr, offset } => N::Store {
            value: S(value),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I32,
        },
        Op::Store32At { value, address } => N::Store {
            value: S(value),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I32,
        },
        Op::Store64Offset16 { value, ptr, offset } => N::Store {
            value: S(value),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I64,
        },
        Op::Store64At { value, address } => N::Store {
            value: S(value),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I64,
        },
        Op::I32StoreOffset16Imm16 { value, ptr, offset } => N::Store {
            value: C(i64::from(i32::from(value))),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I32,
        },
        Op::I32StoreAtImm16 { value, address } => N::Store {
            value: C(i64::from(i32::from(value))),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I32,
        },
        Op::I32Store8Offset16 { value, ptr, offset } => N::Store {
            value: S(value),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I8,
        },
        Op::I32Store8Offset16Imm { value, ptr, offset } => N::Store {
            value: C(i64::from(value)),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I8,
        },
        Op::I32Store8At { value, address } => N::Store {
            value: S(value),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I8,
        },
        Op::I32Store8AtImm { value, address } => N::Store {
            value: C(i64::from(value)),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I8,
        },
        Op::I32Store16Offset16 { value, ptr, offset } => N::Store {
            value: S(value),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I16,
        },
        Op::I32Store16Offset16Imm { value, ptr, offset } => N::Store {
            value: C(i64::from(value)),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I16,
        },
        Op::I32Store16At { value, address } => N::Store {
            value: S(value),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I16,
        },
        Op::I32Store16AtImm { value, address } => N::Store {
            value: C(i64::from(value)),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I16,
        },
        Op::I64StoreOffset16Imm16 { value, ptr, offset } => N::Store {
            value: C(i64::from(value)),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I64,
        },
        Op::I64StoreAtImm16 { value, address } => N::Store {
            value: C(i64::from(value)),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I64,
        },
        Op::I64Store8Offset16 { value, ptr, offset } => N::Store {
            value: S(value),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I8,
        },
        Op::I64Store8Offset16Imm { value, ptr, offset } => N::Store {
            value: C(i64::from(value)),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I8,
        },
        Op::I64Store8At { value, address } => N::Store {
            value: S(value),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I8,
        },
        Op::I64Store8AtImm { value, address } => N::Store {
            value: C(i64::from(value)),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I8,
        },
        Op::I64Store16Offset16 { value, ptr, offset } => N::Store {
            value: S(value),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I16,
        },
        Op::I64Store16Offset16Imm { value, ptr, offset } => N::Store {
            value: C(i64::from(value)),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I16,
        },
        Op::I64Store16At { value, address } => N::Store {
            value: S(value),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I16,
        },
        Op::I64Store16AtImm { value, address } => N::Store {
            value: C(i64::from(value)),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I16,
        },
        Op::I64Store32Offset16 { value, ptr, offset } => N::Store {
            value: S(value),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I32,
        },
        Op::I64Store32Offset16Imm16 { value, ptr, offset } => N::Store {
            value: C(i64::from(i32::from(value))),
            ptr: S(ptr),
            offset: u64::from(crate::ir::Offset64::from(offset)) as u32,
            width: types::I32,
        },
        Op::I64Store32At { value, address } => N::Store {
            value: S(value),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I32,
        },
        Op::I64Store32AtImm16 { value, address } => N::Store {
            value: C(i64::from(i32::from(value))),
            ptr: C(usize::from(address) as i64),
            offset: 0,
            width: types::I32,
        },
        _ => return None,
    })
}

fn decode_select(instrs: &[Op], index: usize) -> Option<NativeOp> {
    let Op::Slot2 { slots: values } = *instrs.get(index + 1)? else {
        return None;
    };
    let (result, comparison) = match instrs[index] {
        Op::SelectI32Eq { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::Equal,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI32EqImm16 { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::Equal,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(i64::from(i32::from(rhs))),
            },
        ),
        Op::SelectI32LtS { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThan,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI32LtSImm16Rhs { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThan,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(i64::from(i32::from(rhs))),
            },
        ),
        Op::SelectI32LtU { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThan,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI32LtUImm16Rhs { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThan,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(i64::from(u32::from(rhs))),
            },
        ),
        Op::SelectI32LeS { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI32LeSImm16Rhs { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(i64::from(i32::from(rhs))),
            },
        ),
        Op::SelectI32LeU { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI32LeUImm16Rhs { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I32,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(i64::from(u32::from(rhs))),
            },
        ),
        Op::SelectI64Eq { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::Equal,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI64EqImm16 { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::Equal,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(i64::from(rhs)),
            },
        ),
        Op::SelectI64LtS { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThan,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI64LtSImm16Rhs { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThan,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(i64::from(rhs)),
            },
        ),
        Op::SelectI64LtU { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThan,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI64LtUImm16Rhs { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThan,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(u64::from(rhs) as i64),
            },
        ),
        Op::SelectI64LeS { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI64LeSImm16Rhs { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::SignedLessThanOrEqual,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(i64::from(rhs)),
            },
        ),
        Op::SelectI64LeU { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Slot(rhs),
            },
        ),
        Op::SelectI64LeUImm16Rhs { result, lhs, rhs } => (
            result,
            Comparison {
                ty: types::I64,
                cc: IntCC::UnsignedLessThanOrEqual,
                lhs: Operand::Slot(lhs),
                rhs: Operand::Constant(u64::from(rhs) as i64),
            },
        ),
        _ => return None,
    };
    Some(NativeOp::Select {
        result,
        comparison,
        values,
    })
}

fn compile(instrs: &[Op], start: usize) -> Option<NativeRegion> {
    let mut ops = Vec::new();
    let mut i = start;
    while i < instrs.len() && ops.len() < MAX_INSTRUCTIONS {
        if let Some(op) = decode_select(instrs, i) {
            if ops.len() + 2 > MAX_INSTRUCTIONS {
                break;
            }
            ops.extend([op, NativeOp::Parameter]);
            i += 2;
            continue;
        }
        let Some(op) = decode(instrs[i]) else { break };
        // Constant-address operations may carry a second word selecting a
        // non-default memory. Leave that complete instruction to Wasmi.
        if matches!(op, NativeOp::Load { .. } | NativeOp::Store { .. })
            && matches!(instrs.get(i + 1), Some(Op::MemoryIndex { .. }))
        {
            break;
        }
        ops.push(op);
        i += 1;
    }
    if ops.len() < 4 {
        return None;
    }
    let mut globals = Vec::new();
    for op in &ops {
        match *op {
            NativeOp::GlobalGet { global, .. }
            | NativeOp::GlobalSet { global, .. }
            | NativeOp::GlobalConstant { global, .. }
                if !globals.contains(&global) =>
            {
                globals.push(global);
            }
            _ => (),
        }
    }
    if globals.len() > MAX_GLOBALS {
        return None;
    }
    for (i, op) in ops.iter().enumerate() {
        if let NativeOp::Branch { offset, .. } = op {
            let target = start as i64 + i as i64 + i64::from(*offset);
            if target < 0 || target >= instrs.len() as i64 {
                return None;
            }
            if let Some(NativeOp::Parameter) = target
                .checked_sub(start as i64)
                .and_then(|index| ops.get(index as usize))
            {
                return None;
            }
        }
    }
    // Falling through the final instruction must also remain in the function.
    if start + ops.len() >= instrs.len() {
        return None;
    }

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
    signature.params.extend([
        AbiParam::new(types::I64),
        AbiParam::new(types::I64),
        AbiParam::new(types::I64),
    ]);
    signature.returns.push(AbiParam::new(types::I64));
    let id = module
        .declare_function("wasmi_region", Linkage::Local, &signature)
        .ok()?;
    ctx.func.signature = signature;
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    let mut function_ctx = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut function_ctx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let slots_ptr = b.block_params(entry)[0];
        let slots = Slots::new(&mut b, slots_ptr, &ops);
        let global_array = b.block_params(entry)[1];
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
        let budget = b.declare_var(types::I32);
        let limit = b.ins().iconst(types::I32, BACKEDGE_BUDGET);
        b.def_var(budget, limit);
        let blocks: Vec<_> = (0..ops.len()).map(|_| b.create_block()).collect();
        let mut exits = BTreeMap::new();
        b.ins().jump(blocks[0], &[]);
        for (i, op) in ops.iter().enumerate() {
            b.switch_to_block(blocks[i]);
            match *op {
                NativeOp::Copy { result, value } => slots.copy(&mut b, result, value),
                NativeOp::Constant { result, value } => {
                    let value = b.ins().iconst(types::I64, value);
                    slots.write(&mut b, result, value);
                    slots.zero_high(&mut b, result);
                }
                NativeOp::GlobalGet { result, global } => {
                    let ptr = global_ptrs[globals.iter().position(|g| *g == global).unwrap()];
                    slots.load(&mut b, result, ptr);
                }
                NativeOp::GlobalSet { input, global } => {
                    let ptr = global_ptrs[globals.iter().position(|g| *g == global).unwrap()];
                    slots.store(&mut b, input, ptr, 0);
                }
                NativeOp::GlobalConstant { value, global } => {
                    let ptr = global_ptrs[globals.iter().position(|g| *g == global).unwrap()];
                    let value = b.ins().iconst(types::I64, value);
                    b.ins().store(MemFlags::trusted(), value, ptr, 0);
                    zero_high(&mut b, ptr, 0);
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
                NativeOp::Convert {
                    result,
                    input,
                    from,
                    to,
                    signed,
                } => {
                    let value = slots.read(&mut b, from, Operand::Slot(input));
                    let value = if from == to {
                        value
                    } else if signed {
                        b.ins().sextend(to, value)
                    } else {
                        b.ins().uextend(to, value)
                    };
                    slots.write(&mut b, result, value);
                }
                NativeOp::Select {
                    result,
                    comparison,
                    values,
                } => {
                    let condition = compare(&mut b, &slots, comparison);
                    for word in 0..VALUE_WORDS {
                        let yes = b.use_var(slots.variables[&i16::from(values[0])][word]);
                        let no = b.use_var(slots.variables[&i16::from(values[1])][word]);
                        let value = b.ins().select(condition, yes, no);
                        b.def_var(slots.variables[&i16::from(result)][word], value);
                    }
                    let next =
                        destination(&mut b, &blocks, &mut exits, start, (start + i + 2) as isize);
                    b.ins().jump(next, &[]);
                    continue;
                }
                NativeOp::Parameter => {
                    // No validated control-flow edge targets an operand word.
                    // Keep an unreachable block so branch offsets still index
                    // the original Wasmi instruction array exactly.
                    let value = b.ins().iconst(types::I64, (start + i) as i64);
                    b.ins().return_(&[value]);
                    continue;
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
                        start + i,
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
                        start + i,
                        MemoryAccess { ptr, offset, width },
                    );
                    let value = slots.read(&mut b, width, value);
                    b.ins()
                        .store(MemFlags::new().with_notrap(), value, address, 0);
                    memory.mark_dirty(&mut b, relative, width);
                }
                NativeOp::Branch { comparison, offset } => {
                    let target = start as isize + i as isize + offset as isize;
                    let to = edge(&mut b, &blocks, &mut exits, start, i, target, budget);
                    if let Some(comparison) = comparison {
                        let condition = compare(&mut b, &slots, comparison);
                        let next = destination(
                            &mut b,
                            &blocks,
                            &mut exits,
                            start,
                            (start + i + 1) as isize,
                        );
                        b.ins().brif(condition, to, &[], next, &[]);
                    } else {
                        b.ins().jump(to, &[]);
                    }
                    continue;
                }
            }
            let next = destination(&mut b, &blocks, &mut exits, start, (start + i + 1) as isize);
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
        b.finalize(module.target_config());
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
        unsafe { module.free_memory() };
        return None;
    }
    let code = module.get_finalized_function(id);
    let code_bytes = ctx
        .compiled_code()
        .map_or(0, |code| code.code_buffer().len());
    // SAFETY: the generated signature exactly matches this C ABI. Its owner is
    // retained in the returned region until no invocation can remain live.
    let entry = unsafe {
        core::mem::transmute::<
            *const u8,
            unsafe extern "C" fn(
                *mut UntypedVal,
                *const *mut UntypedVal,
                *mut NativeMemory,
            ) -> usize,
        >(code)
    };
    Some(NativeRegion {
        _owner: CodeOwner(Mutex::new(Some(module))),
        entry,
        globals,
        function_base: instrs.as_ptr() as usize,
        instructions: ops.len(),
        code_bytes,
    })
}

fn slot_offset(slot: Slot) -> i32 {
    i32::from(i16::from(slot)) * core::mem::size_of::<UntypedVal>() as i32
}

const VALUE_WORDS: usize = core::mem::size_of::<UntypedVal>() / 8;

struct Slots {
    pointer: Value,
    variables: BTreeMap<i16, [Variable; VALUE_WORDS]>,
    dirty: BTreeSet<i16>,
}

impl Slots {
    fn new(b: &mut FunctionBuilder<'_>, pointer: Value, ops: &[NativeOp]) -> Self {
        let mut all = BTreeSet::new();
        let mut dirty = BTreeSet::new();
        for op in ops {
            op.visit_slots(|slot, written| {
                all.insert(i16::from(slot));
                if written {
                    dirty.insert(i16::from(slot));
                }
            });
        }
        let variables = all
            .into_iter()
            .map(|slot| {
                let words = core::array::from_fn(|word| {
                    let variable = b.declare_var(types::I64);
                    let value = b.ins().load(
                        types::I64,
                        MemFlags::trusted(),
                        pointer,
                        i32::from(slot) * core::mem::size_of::<UntypedVal>() as i32
                            + word as i32 * 8,
                    );
                    b.def_var(variable, value);
                    variable
                });
                (slot, words)
            })
            .collect();
        Self {
            pointer,
            variables,
            dirty,
        }
    }

    fn read(&self, b: &mut FunctionBuilder<'_>, ty: Type, operand: Operand) -> Value {
        let Operand::Slot(slot) = operand else {
            let Operand::Constant(value) = operand else {
                unreachable!()
            };
            return b.ins().iconst(ty, value);
        };
        let value = b.use_var(self.variables[&i16::from(slot)][0]);
        if ty == types::I64 {
            value
        } else {
            b.ins().ireduce(ty, value)
        }
    }

    fn write(&self, b: &mut FunctionBuilder<'_>, result: Slot, value: Value) {
        let value = if b.func.dfg.value_type(value) != types::I64 {
            b.ins().uextend(types::I64, value)
        } else {
            value
        };
        b.def_var(self.variables[&i16::from(result)][0], value);
    }

    fn zero_high(&self, b: &mut FunctionBuilder<'_>, result: Slot) {
        for word in &self.variables[&i16::from(result)][1..] {
            let zero = b.ins().iconst(types::I64, 0);
            b.def_var(*word, zero);
        }
    }

    fn copy(&self, b: &mut FunctionBuilder<'_>, result: Slot, value: Slot) {
        for (dst, src) in self.variables[&i16::from(result)]
            .iter()
            .zip(&self.variables[&i16::from(value)])
        {
            let value = b.use_var(*src);
            b.def_var(*dst, value);
        }
    }

    fn load(&self, b: &mut FunctionBuilder<'_>, result: Slot, ptr: Value) {
        for (i, variable) in self.variables[&i16::from(result)].iter().enumerate() {
            let value = b
                .ins()
                .load(types::I64, MemFlags::trusted(), ptr, i as i32 * 8);
            b.def_var(*variable, value);
        }
    }

    fn store(&self, b: &mut FunctionBuilder<'_>, input: Slot, ptr: Value, offset: i32) {
        for (i, variable) in self.variables[&i16::from(input)].iter().enumerate() {
            let value = b.use_var(*variable);
            b.ins()
                .store(MemFlags::trusted(), value, ptr, offset + i as i32 * 8);
        }
    }

    fn flush(&self, b: &mut FunctionBuilder<'_>) {
        for slot in &self.dirty {
            self.store(
                b,
                Slot::from(*slot),
                self.pointer,
                slot_offset(Slot::from(*slot)),
            );
        }
    }
}

impl NativeOp {
    fn visit_slots(&self, mut visit: impl FnMut(Slot, bool)) {
        use NativeOp as N;
        let (result, lhs, rhs) = match *self {
            N::Copy { result, value } => (Some(result), Some(Operand::Slot(value)), None),
            N::Constant { result, .. } | N::GlobalGet { result, .. } => (Some(result), None, None),
            N::GlobalSet { input, .. } => (None, Some(Operand::Slot(input)), None),
            N::GlobalConstant { .. } => (None, None, None),
            N::Binary {
                result, lhs, rhs, ..
            } => (Some(result), Some(lhs), Some(rhs)),
            N::Compare { result, comparison } => {
                (Some(result), Some(comparison.lhs), Some(comparison.rhs))
            }
            N::Branch {
                comparison: Some(comparison),
                ..
            } => (None, Some(comparison.lhs), Some(comparison.rhs)),
            N::Branch {
                comparison: None, ..
            } => (None, None, None),
            N::Load { result, ptr, .. } => (Some(result), Some(ptr), None),
            N::Store { value, ptr, .. } => (None, Some(value), Some(ptr)),
            N::Convert { result, input, .. } => (Some(result), Some(Operand::Slot(input)), None),
            N::Select {
                result,
                comparison,
                values,
            } => {
                for value in values {
                    visit(value, false);
                }
                (Some(result), Some(comparison.lhs), Some(comparison.rhs))
            }
            N::Parameter => (None, None, None),
        };
        if let Some(result) = result {
            visit(result, true);
        }
        for operand in [lhs, rhs].into_iter().flatten() {
            if let Operand::Slot(slot) = operand {
                visit(slot, false);
            }
        }
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

    fn checked_address(
        &self,
        b: &mut FunctionBuilder<'_>,
        slots: &Slots,
        exits: &mut BTreeMap<isize, Block>,
        instruction: usize,
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
        let exit = *exits
            .entry(instruction as isize)
            .or_insert_with(|| b.create_block());
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

fn zero_high(b: &mut FunctionBuilder<'_>, ptr: Value, offset: i32) {
    #[cfg(feature = "simd")]
    {
        let zero = b.ins().iconst(types::I64, 0);
        b.ins().store(MemFlags::trusted(), zero, ptr, offset + 8);
    }
    #[cfg(not(feature = "simd"))]
    let _ = (b, ptr, offset);
}

fn compare(b: &mut FunctionBuilder<'_>, slots: &Slots, comparison: Comparison) -> Value {
    let lhs = slots.read(b, comparison.ty, comparison.lhs);
    let rhs = slots.read(b, comparison.ty, comparison.rhs);
    b.ins().icmp(comparison.cc, lhs, rhs)
}

fn destination(
    b: &mut FunctionBuilder<'_>,
    blocks: &[Block],
    exits: &mut BTreeMap<isize, Block>,
    start: usize,
    target: isize,
) -> Block {
    if target >= start as isize && target < (start + blocks.len()) as isize {
        return blocks[target as usize - start];
    }
    *exits.entry(target).or_insert_with(|| b.create_block())
}

fn edge(
    b: &mut FunctionBuilder<'_>,
    blocks: &[Block],
    exits: &mut BTreeMap<isize, Block>,
    start: usize,
    current: usize,
    target: isize,
    budget: Variable,
) -> Block {
    let to = destination(b, blocks, exits, start, target);
    if target > (start + current) as isize || target < start as isize {
        return to;
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
