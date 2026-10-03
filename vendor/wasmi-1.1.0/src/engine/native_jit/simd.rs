//! Native lowering of WebAssembly 128-bit vector instructions.
//!
//! WebAssembly Core (local official snapshot 37d6b059, 2026-09-06):
//! #syntax-instr-vec, #exec-instr-vec and the vector numerics of
//! `specification/wasm-latest/3.2-numerics.vector.spectec`.
//!
//! * Lanes are numbered from the least significant byte of the little-endian
//!   v128. A frame slot (`UntypedVal { lo64, hi64 }`) and linear memory both
//!   store lane `i` of an `i8x16` at byte `i`, so 16-byte loads and stores
//!   need no lane reordering on the little-endian hosts that run native code.
//! * Shift counts are taken modulo the lane width (#op-ishl, #op-ishr), which
//!   is also Cranelift's vector shift semantics.
//! * Saturating arithmetic and narrowing clamp to the destination lane range;
//!   `narrow_u` interprets its input lanes as signed (#op-narrow).
//! * Float operations may return any NaN permitted by #aux-nans; `min`/`max`
//!   propagate NaN and order -0 below +0, unlike `pmin`/`pmax`, which are
//!   defined by a single `<` comparison (#op-fpmin).
//! * v128 memory accesses have no alignment requirement; the memory argument's
//!   alignment is only a hint, so no access carries Cranelift's `aligned` flag.
//!
//! Relaxed SIMD and loads or stores with non-default memories or offsets above
//! `u32::MAX` remain interpreter boundaries.

use super::{MemoryAccess, MemoryCode, NativeOp, Operand, Slots};
use crate::{
    V128,
    core::UntypedVal,
    ir::{Offset64, Op, Slot},
};
use cranelift_codegen::ir::{
    Block, ConstantData, InstBuilder, MemFlagsData as MemFlags, Type, Value,
    condcodes::{FloatCC, IntCC},
    types::{F32, F32X4, F64, F64X2, I8, I8X16, I16, I16X8, I32, I32X4, I64, I64X2},
};
use cranelift_frontend::FunctionBuilder;
use std::collections::BTreeMap;

/// A lane-wise operation on two vectors of lane type `ty`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Binary {
    Add,
    Sub,
    Mul,
    AddSat {
        signed: bool,
    },
    SubSat {
        signed: bool,
    },
    Min {
        signed: bool,
    },
    Max {
        signed: bool,
    },
    AvgrU,
    Q15MulrSatS,
    And,
    Or,
    Xor,
    AndNot,
    Swizzle,
    Icmp(IntCC),
    Fcmp(FloatCC),
    FAdd,
    FSub,
    FMul,
    FDiv,
    FMin,
    FMax,
    FPmin,
    FPmax,
    /// `ty` is the wider source lane type.
    Narrow {
        signed: bool,
    },
    /// `ty` is the narrower source lane type.
    ExtMul {
        high: bool,
        signed: bool,
    },
    /// `i32x4.dot_i16x8_s`; `ty` is `i16x8`.
    Dot,
}

/// A lane-wise operation on one vector of lane type `ty`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Unary {
    Not,
    Neg,
    Abs,
    Popcnt,
    FNeg,
    FAbs,
    Sqrt,
    Ceil,
    Floor,
    Trunc,
    Nearest,
    /// `ty` is the narrower source lane type.
    Extend {
        high: bool,
        signed: bool,
    },
    /// `ty` is the narrower source lane type.
    ExtAddPairwise {
        signed: bool,
    },
    /// `f32x4.convert_i32x4_{s,u}`.
    ConvertI32 {
        signed: bool,
    },
    /// `f64x2.convert_low_i32x4_{s,u}`.
    ConvertLowI32 {
        signed: bool,
    },
    /// `i32x4.trunc_sat_f32x4_{s,u}`.
    TruncSatF32 {
        signed: bool,
    },
    /// `i32x4.trunc_sat_f64x2_{s,u}_zero`.
    TruncSatF64Zero {
        signed: bool,
    },
    /// `f32x4.demote_f64x2_zero`.
    Demote,
    /// `f64x2.promote_low_f32x4`.
    Promote,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Shift {
    Shl,
    ShrS,
    ShrU,
}

/// A vector-to-`i32` reduction.
#[derive(Clone, Copy, Debug)]
pub(super) enum Test {
    AnyTrue,
    AllTrue,
    Bitmask,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Load {
    /// `v128.load`.
    Full,
    /// `v128.loadN_splat`; the scalar lane type.
    Splat(Type),
    /// `v128.loadN_zero`; the scalar lane type.
    Zero(Type),
    /// `v128.loadMxN_{s,u}`; the narrow source lane type.
    Extend { lane: Type, signed: bool },
    /// `v128.loadN_lane`, replacing `lane` of `input`.
    Lane { ty: Type, lane: u8, input: Slot },
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Store {
    /// `v128.store`.
    Full,
    /// `v128.storeN_lane`.
    Lane { ty: Type, lane: u8 },
}

#[derive(Clone, Copy, Debug)]
pub(super) enum VectorOp {
    Binary {
        result: Slot,
        kind: Binary,
        ty: Type,
        lhs: Slot,
        rhs: Slot,
    },
    Unary {
        result: Slot,
        kind: Unary,
        ty: Type,
        input: Slot,
    },
    /// `v128.bitselect(lhs, rhs, mask)`.
    Bitselect {
        result: Slot,
        lhs: Slot,
        rhs: Slot,
        mask: Slot,
    },
    Shuffle {
        result: Slot,
        lhs: Slot,
        rhs: Slot,
        lanes: [u8; 16],
    },
    Shift {
        result: Slot,
        kind: Shift,
        ty: Type,
        lhs: Slot,
        amount: Operand,
    },
    Splat {
        result: Slot,
        ty: Type,
        value: Slot,
    },
    /// Extracts `lane` of `input`; `signed` selects `extract_lane_s` for
    /// 8- and 16-bit lanes.
    Extract {
        result: Slot,
        ty: Type,
        lane: u8,
        input: Slot,
        signed: bool,
    },
    Replace {
        result: Slot,
        ty: Type,
        lane: u8,
        input: Slot,
        value: Operand,
    },
    Test {
        result: Slot,
        kind: Test,
        ty: Type,
        input: Slot,
    },
    Load {
        result: Slot,
        ptr: Operand,
        offset: u32,
        kind: Load,
    },
    Store {
        value: Slot,
        ptr: Operand,
        offset: u32,
        kind: Store,
    },
}

impl VectorOp {
    /// Visits `(slot, written, holds_v128)` for every frame slot the
    /// operation accesses. Scalar operands report `holds_v128 == false`.
    pub(super) fn visit_slots(&self, mut visit: impl FnMut(Slot, bool, bool)) {
        let operand = |operand: Operand, visit: &mut dyn FnMut(Slot, bool, bool)| {
            if let Operand::Slot(slot) = operand {
                visit(slot, false, false);
            }
        };
        match *self {
            VectorOp::Binary {
                result, lhs, rhs, ..
            }
            | VectorOp::Shuffle {
                result, lhs, rhs, ..
            } => {
                visit(lhs, false, true);
                visit(rhs, false, true);
                visit(result, true, true);
            }
            VectorOp::Unary { result, input, .. } => {
                visit(input, false, true);
                visit(result, true, true);
            }
            VectorOp::Bitselect {
                result,
                lhs,
                rhs,
                mask,
            } => {
                visit(lhs, false, true);
                visit(rhs, false, true);
                visit(mask, false, true);
                visit(result, true, true);
            }
            VectorOp::Shift {
                result,
                lhs,
                amount,
                ..
            } => {
                visit(lhs, false, true);
                operand(amount, &mut visit);
                visit(result, true, true);
            }
            VectorOp::Splat { result, value, .. } => {
                visit(value, false, false);
                visit(result, true, true);
            }
            VectorOp::Extract { result, input, .. } => {
                visit(input, false, true);
                visit(result, true, false);
            }
            VectorOp::Replace {
                result,
                input,
                value,
                ..
            } => {
                visit(input, false, true);
                operand(value, &mut visit);
                visit(result, true, true);
            }
            VectorOp::Test { result, input, .. } => {
                visit(input, false, true);
                visit(result, true, false);
            }
            VectorOp::Load {
                result, ptr, kind, ..
            } => {
                operand(ptr, &mut visit);
                if let Load::Lane { input, .. } = kind {
                    visit(input, false, true);
                }
                visit(result, true, true);
            }
            VectorOp::Store { value, ptr, .. } => {
                visit(value, false, true);
                operand(ptr, &mut visit);
            }
        }
    }
}

/// Lane `i` of the shuffle selector constant: the translator stores the
/// immediate lane indices as a function-local constant in a negative slot.
fn shuffle_lanes(consts: &[UntypedVal], selector: Slot) -> Option<[u8; 16]> {
    let index = consts
        .len()
        .checked_add_signed(isize::from(i16::from(selector)))?;
    if i16::from(selector) >= 0 || index >= consts.len() {
        return None;
    }
    // The translator encodes the selector with `u128::from_ne_bytes`.
    let lanes = V128::from(consts[index]).as_u128().to_ne_bytes();
    lanes.iter().all(|lane| *lane < 32).then_some(lanes)
}

fn binary(result: Slot, kind: Binary, ty: Type, lhs: Slot, rhs: Slot) -> VectorOp {
    VectorOp::Binary {
        result,
        kind,
        ty,
        lhs,
        rhs,
    }
}

fn unary(result: Slot, kind: Unary, ty: Type, input: Slot) -> VectorOp {
    VectorOp::Unary {
        result,
        kind,
        ty,
        input,
    }
}

fn shift(result: Slot, kind: Shift, ty: Type, lhs: Slot, amount: Operand) -> VectorOp {
    VectorOp::Shift {
        result,
        kind,
        ty,
        lhs,
        amount,
    }
}

fn test(result: Slot, kind: Test, ty: Type, input: Slot) -> VectorOp {
    VectorOp::Test {
        result,
        kind,
        ty,
        input,
    }
}

fn load_at(result: Slot, ptr: Slot, offset: impl Into<Offset64>, kind: Load) -> VectorOp {
    VectorOp::Load {
        result,
        ptr: Operand::Slot(ptr),
        offset: u64::from(offset.into()) as u32,
        kind,
    }
}

fn load_constant(result: Slot, address: crate::ir::Address32, kind: Load) -> VectorOp {
    VectorOp::Load {
        result,
        ptr: Operand::Constant(usize::from(address) as i64),
        offset: 0,
        kind,
    }
}

/// The 32-bit offset of a full-width memory instruction whose high offset word
/// follows it, or `None` when the offset does not fit the native bounds check.
fn split_offset(lo: crate::ir::Offset64Lo, param: Option<Op>) -> Option<(Slot, u32)> {
    let (slot, hi) = param?.filter_register_and_offset_hi().ok()?;
    let offset = u64::from(Offset64::combine(hi, lo));
    Some((slot, u32::try_from(offset).ok()?))
}

/// Decodes the v128 instruction at `index` and the number of instruction
/// words it occupies, or `None` if it must remain an interpreter boundary.
pub(super) fn decode(
    instrs: &[Op],
    index: usize,
    consts: &[UntypedVal],
) -> Option<(NativeOp, usize)> {
    use Binary as B;
    use Unary as U;
    let word = |delta: usize| instrs.get(index + delta).copied();
    let constant = Operand::Constant;
    // Only the default memory is available to native code. A trailing
    // `MemoryIndex` word selects another memory, even when it is index 0.
    let default_memory = |delta: usize| !matches!(word(delta), Some(Op::MemoryIndex { .. }));
    let (vector, words) = match instrs[index] {
        Op::I8x16Shuffle { result, lhs, rhs } => {
            let Some(Op::Slot { slot }) = word(1) else {
                return None;
            };
            let lanes = shuffle_lanes(consts, slot)?;
            (
                VectorOp::Shuffle {
                    result,
                    lhs,
                    rhs,
                    lanes,
                },
                2,
            )
        }
        Op::V128Bitselect { result, lhs, rhs } => {
            let Some(Op::Slot { slot: mask }) = word(1) else {
                return None;
            };
            (
                VectorOp::Bitselect {
                    result,
                    lhs,
                    rhs,
                    mask,
                },
                2,
            )
        }
        Op::I8x16ReplaceLane {
            result,
            input,
            lane,
        } => replace_slot(result, input, I8X16, u8::from(lane), word(1))?,
        Op::I16x8ReplaceLane {
            result,
            input,
            lane,
        } => replace_slot(result, input, I16X8, u8::from(lane), word(1))?,
        Op::I32x4ReplaceLane {
            result,
            input,
            lane,
        } => replace_slot(result, input, I32X4, u8::from(lane), word(1))?,
        Op::I64x2ReplaceLane {
            result,
            input,
            lane,
        } => replace_slot(result, input, I64X2, u8::from(lane), word(1))?,
        Op::F32x4ReplaceLane {
            result,
            input,
            lane,
        } => replace_slot(result, input, F32X4, u8::from(lane), word(1))?,
        Op::F64x2ReplaceLane {
            result,
            input,
            lane,
        } => replace_slot(result, input, F64X2, u8::from(lane), word(1))?,
        Op::I8x16ReplaceLaneImm {
            result,
            input,
            lane,
            value,
        } => (
            VectorOp::Replace {
                result,
                ty: I8X16,
                lane: u8::from(lane),
                input,
                value: constant(i64::from(value)),
            },
            1,
        ),
        Op::I16x8ReplaceLaneImm {
            result,
            input,
            lane,
        } => {
            let Some(Op::Const32 { value }) = word(1) else {
                return None;
            };
            replace_constant(
                result,
                input,
                I16X8,
                u8::from(lane),
                i32::from(value).into(),
            )
        }
        Op::I32x4ReplaceLaneImm {
            result,
            input,
            lane,
        } => {
            let Some(Op::Const32 { value }) = word(1) else {
                return None;
            };
            replace_constant(
                result,
                input,
                I32X4,
                u8::from(lane),
                i32::from(value).into(),
            )
        }
        Op::I64x2ReplaceLaneImm32 {
            result,
            input,
            lane,
        } => {
            let Some(Op::I64Const32 { value }) = word(1) else {
                return None;
            };
            replace_constant(result, input, I64X2, u8::from(lane), i64::from(value))
        }
        Op::F32x4ReplaceLaneImm {
            result,
            input,
            lane,
        } => {
            let Some(Op::Const32 { value }) = word(1) else {
                return None;
            };
            let bits = f32::from(value).to_bits();
            replace_constant(result, input, F32X4, u8::from(lane), bits.into())
        }
        Op::F64x2ReplaceLaneImm32 {
            result,
            input,
            lane,
        } => {
            let Some(Op::F64Const32 { value }) = word(1) else {
                return None;
            };
            let bits = f64::from(value).to_bits() as i64;
            replace_constant(result, input, F64X2, u8::from(lane), bits)
        }

        // Memory: only the 16-bit offset, constant address and (for lane
        // loads) general forms that address the default memory.
        Op::V128LoadOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, Load::Full), 1),
        Op::V128LoadAt { result, address } if default_memory(1) => {
            (load_constant(result, address, Load::Full), 1)
        }
        Op::V128Load { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), Load::Full)?
        }
        Op::V128Load8Splat { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), Load::Splat(I8))?
        }
        Op::V128Load16Splat { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), Load::Splat(I16))?
        }
        Op::V128Load32Splat { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), Load::Splat(I32))?
        }
        Op::V128Load64Splat { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), Load::Splat(I64))?
        }
        Op::V128Load32Zero { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), Load::Zero(I32))?
        }
        Op::V128Load64Zero { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), Load::Zero(I64))?
        }
        Op::V128Load8x8S { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), extend_load(I8, true))?
        }
        Op::V128Load8x8U { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), extend_load(I8, false))?
        }
        Op::V128Load16x4S { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), extend_load(I16, true))?
        }
        Op::V128Load16x4U { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), extend_load(I16, false))?
        }
        Op::V128Load32x2S { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), extend_load(I32, true))?
        }
        Op::V128Load32x2U { result, offset_lo } if default_memory(2) => {
            general_load(result, offset_lo, word(1), extend_load(I32, false))?
        }
        Op::V128Load8SplatOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, Load::Splat(I8)), 1),
        Op::V128Load16SplatOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, Load::Splat(I16)), 1),
        Op::V128Load32SplatOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, Load::Splat(I32)), 1),
        Op::V128Load64SplatOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, Load::Splat(I64)), 1),
        Op::V128Load8SplatAt { result, address } if default_memory(1) => {
            (load_constant(result, address, Load::Splat(I8)), 1)
        }
        Op::V128Load16SplatAt { result, address } if default_memory(1) => {
            (load_constant(result, address, Load::Splat(I16)), 1)
        }
        Op::V128Load32SplatAt { result, address } if default_memory(1) => {
            (load_constant(result, address, Load::Splat(I32)), 1)
        }
        Op::V128Load64SplatAt { result, address } if default_memory(1) => {
            (load_constant(result, address, Load::Splat(I64)), 1)
        }
        Op::V128Load32ZeroOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, Load::Zero(I32)), 1),
        Op::V128Load64ZeroOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, Load::Zero(I64)), 1),
        Op::V128Load32ZeroAt { result, address } if default_memory(1) => {
            (load_constant(result, address, Load::Zero(I32)), 1)
        }
        Op::V128Load64ZeroAt { result, address } if default_memory(1) => {
            (load_constant(result, address, Load::Zero(I64)), 1)
        }
        Op::V128Load8x8SOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, extend_load(I8, true)), 1),
        Op::V128Load8x8UOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, extend_load(I8, false)), 1),
        Op::V128Load16x4SOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, extend_load(I16, true)), 1),
        Op::V128Load16x4UOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, extend_load(I16, false)), 1),
        Op::V128Load32x2SOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, extend_load(I32, true)), 1),
        Op::V128Load32x2UOffset16 {
            result,
            ptr,
            offset,
        } => (load_at(result, ptr, offset, extend_load(I32, false)), 1),
        Op::V128Load8x8SAt { result, address } if default_memory(1) => {
            (load_constant(result, address, extend_load(I8, true)), 1)
        }
        Op::V128Load8x8UAt { result, address } if default_memory(1) => {
            (load_constant(result, address, extend_load(I8, false)), 1)
        }
        Op::V128Load16x4SAt { result, address } if default_memory(1) => {
            (load_constant(result, address, extend_load(I16, true)), 1)
        }
        Op::V128Load16x4UAt { result, address } if default_memory(1) => {
            (load_constant(result, address, extend_load(I16, false)), 1)
        }
        Op::V128Load32x2SAt { result, address } if default_memory(1) => {
            (load_constant(result, address, extend_load(I32, true)), 1)
        }
        Op::V128Load32x2UAt { result, address } if default_memory(1) => {
            (load_constant(result, address, extend_load(I32, false)), 1)
        }
        Op::V128Load8Lane { result, offset_lo } if default_memory(3) => {
            load_lane(result, I8, offset_lo, word(1), word(2))?
        }
        Op::V128Load16Lane { result, offset_lo } if default_memory(3) => {
            load_lane(result, I16, offset_lo, word(1), word(2))?
        }
        Op::V128Load32Lane { result, offset_lo } if default_memory(3) => {
            load_lane(result, I32, offset_lo, word(1), word(2))?
        }
        Op::V128Load64Lane { result, offset_lo } if default_memory(3) => {
            load_lane(result, I64, offset_lo, word(1), word(2))?
        }
        Op::V128Load8LaneAt { result, address } if default_memory(2) => {
            load_lane_at(result, I8, address, word(1))?
        }
        Op::V128Load16LaneAt { result, address } if default_memory(2) => {
            load_lane_at(result, I16, address, word(1))?
        }
        Op::V128Load32LaneAt { result, address } if default_memory(2) => {
            load_lane_at(result, I32, address, word(1))?
        }
        Op::V128Load64LaneAt { result, address } if default_memory(2) => {
            load_lane_at(result, I64, address, word(1))?
        }
        Op::V128StoreOffset16 { ptr, offset, value } => (
            VectorOp::Store {
                value,
                ptr: Operand::Slot(ptr),
                offset: u64::from(Offset64::from(offset)) as u32,
                kind: Store::Full,
            },
            1,
        ),
        Op::V128StoreAt { value, address } if default_memory(1) => (
            VectorOp::Store {
                value,
                ptr: Operand::Constant(usize::from(address) as i64),
                offset: 0,
                kind: Store::Full,
            },
            1,
        ),
        Op::V128Store { ptr, offset_lo } if default_memory(2) => {
            let (value, offset) = split_offset(offset_lo, word(1))?;
            (
                VectorOp::Store {
                    value,
                    ptr: Operand::Slot(ptr),
                    offset,
                    kind: Store::Full,
                },
                2,
            )
        }
        Op::V128Store8LaneOffset8 {
            ptr,
            value,
            offset,
            lane,
        } => store_lane(ptr, value, offset, I8, u8::from(lane)),
        Op::V128Store16LaneOffset8 {
            ptr,
            value,
            offset,
            lane,
        } => store_lane(ptr, value, offset, I16, u8::from(lane)),
        Op::V128Store32LaneOffset8 {
            ptr,
            value,
            offset,
            lane,
        } => store_lane(ptr, value, offset, I32, u8::from(lane)),
        Op::V128Store64LaneOffset8 {
            ptr,
            value,
            offset,
            lane,
        } => store_lane(ptr, value, offset, I64, u8::from(lane)),
        Op::V128Store8Lane { ptr, offset_lo } => {
            store_lane_general(ptr, offset_lo, I8, word(1), word(2))?
        }
        Op::V128Store16Lane { ptr, offset_lo } => {
            store_lane_general(ptr, offset_lo, I16, word(1), word(2))?
        }
        Op::V128Store32Lane { ptr, offset_lo } => {
            store_lane_general(ptr, offset_lo, I32, word(1), word(2))?
        }
        Op::V128Store64Lane { ptr, offset_lo } => {
            store_lane_general(ptr, offset_lo, I64, word(1), word(2))?
        }
        Op::V128Store8LaneAt { value, address } => store_lane_at(value, address, I8, word(1))?,
        Op::V128Store16LaneAt { value, address } => store_lane_at(value, address, I16, word(1))?,
        Op::V128Store32LaneAt { value, address } => store_lane_at(value, address, I32, word(1))?,
        Op::V128Store64LaneAt { value, address } => store_lane_at(value, address, I64, word(1))?,

        Op::I8x16Splat { result, value } => splat(result, I8X16, value),
        Op::I16x8Splat { result, value } => splat(result, I16X8, value),
        Op::I32x4Splat { result, value } => splat(result, I32X4, value),
        Op::I64x2Splat { result, value } => splat(result, I64X2, value),
        Op::F32x4Splat { result, value } => splat(result, F32X4, value),
        Op::F64x2Splat { result, value } => splat(result, F64X2, value),

        Op::I8x16ExtractLaneS {
            result,
            value,
            lane,
        } => extract(result, value, I8X16, u8::from(lane), true),
        Op::I8x16ExtractLaneU {
            result,
            value,
            lane,
        } => extract(result, value, I8X16, u8::from(lane), false),
        Op::I16x8ExtractLaneS {
            result,
            value,
            lane,
        } => extract(result, value, I16X8, u8::from(lane), true),
        Op::I16x8ExtractLaneU {
            result,
            value,
            lane,
        } => extract(result, value, I16X8, u8::from(lane), false),
        Op::I32x4ExtractLane {
            result,
            value,
            lane,
        } => extract(result, value, I32X4, u8::from(lane), false),
        Op::I64x2ExtractLane {
            result,
            value,
            lane,
        } => extract(result, value, I64X2, u8::from(lane), false),
        Op::F32x4ExtractLane {
            result,
            value,
            lane,
        } => extract(result, value, F32X4, u8::from(lane), false),
        Op::F64x2ExtractLane {
            result,
            value,
            lane,
        } => extract(result, value, F64X2, u8::from(lane), false),

        Op::I8x16Swizzle {
            result,
            input,
            selector,
        } => (binary(result, B::Swizzle, I8X16, input, selector), 1),

        Op::I8x16Add { result, lhs, rhs } => (binary(result, B::Add, I8X16, lhs, rhs), 1),
        Op::I16x8Add { result, lhs, rhs } => (binary(result, B::Add, I16X8, lhs, rhs), 1),
        Op::I32x4Add { result, lhs, rhs } => (binary(result, B::Add, I32X4, lhs, rhs), 1),
        Op::I64x2Add { result, lhs, rhs } => (binary(result, B::Add, I64X2, lhs, rhs), 1),
        Op::I8x16Sub { result, lhs, rhs } => (binary(result, B::Sub, I8X16, lhs, rhs), 1),
        Op::I16x8Sub { result, lhs, rhs } => (binary(result, B::Sub, I16X8, lhs, rhs), 1),
        Op::I32x4Sub { result, lhs, rhs } => (binary(result, B::Sub, I32X4, lhs, rhs), 1),
        Op::I64x2Sub { result, lhs, rhs } => (binary(result, B::Sub, I64X2, lhs, rhs), 1),
        Op::I16x8Mul { result, lhs, rhs } => (binary(result, B::Mul, I16X8, lhs, rhs), 1),
        Op::I32x4Mul { result, lhs, rhs } => (binary(result, B::Mul, I32X4, lhs, rhs), 1),
        Op::I64x2Mul { result, lhs, rhs } => (binary(result, B::Mul, I64X2, lhs, rhs), 1),
        Op::I32x4DotI16x8S { result, lhs, rhs } => (binary(result, B::Dot, I16X8, lhs, rhs), 1),

        Op::I8x16AddSatS { result, lhs, rhs } => (
            binary(result, B::AddSat { signed: true }, I8X16, lhs, rhs),
            1,
        ),
        Op::I8x16AddSatU { result, lhs, rhs } => (
            binary(result, B::AddSat { signed: false }, I8X16, lhs, rhs),
            1,
        ),
        Op::I16x8AddSatS { result, lhs, rhs } => (
            binary(result, B::AddSat { signed: true }, I16X8, lhs, rhs),
            1,
        ),
        Op::I16x8AddSatU { result, lhs, rhs } => (
            binary(result, B::AddSat { signed: false }, I16X8, lhs, rhs),
            1,
        ),
        Op::I8x16SubSatS { result, lhs, rhs } => (
            binary(result, B::SubSat { signed: true }, I8X16, lhs, rhs),
            1,
        ),
        Op::I8x16SubSatU { result, lhs, rhs } => (
            binary(result, B::SubSat { signed: false }, I8X16, lhs, rhs),
            1,
        ),
        Op::I16x8SubSatS { result, lhs, rhs } => (
            binary(result, B::SubSat { signed: true }, I16X8, lhs, rhs),
            1,
        ),
        Op::I16x8SubSatU { result, lhs, rhs } => (
            binary(result, B::SubSat { signed: false }, I16X8, lhs, rhs),
            1,
        ),
        Op::I16x8Q15MulrSatS { result, lhs, rhs } => {
            (binary(result, B::Q15MulrSatS, I16X8, lhs, rhs), 1)
        }
        Op::I8x16MinS { result, lhs, rhs } => {
            (binary(result, B::Min { signed: true }, I8X16, lhs, rhs), 1)
        }
        Op::I8x16MinU { result, lhs, rhs } => {
            (binary(result, B::Min { signed: false }, I8X16, lhs, rhs), 1)
        }
        Op::I16x8MinS { result, lhs, rhs } => {
            (binary(result, B::Min { signed: true }, I16X8, lhs, rhs), 1)
        }
        Op::I16x8MinU { result, lhs, rhs } => {
            (binary(result, B::Min { signed: false }, I16X8, lhs, rhs), 1)
        }
        Op::I32x4MinS { result, lhs, rhs } => {
            (binary(result, B::Min { signed: true }, I32X4, lhs, rhs), 1)
        }
        Op::I32x4MinU { result, lhs, rhs } => {
            (binary(result, B::Min { signed: false }, I32X4, lhs, rhs), 1)
        }
        Op::I8x16MaxS { result, lhs, rhs } => {
            (binary(result, B::Max { signed: true }, I8X16, lhs, rhs), 1)
        }
        Op::I8x16MaxU { result, lhs, rhs } => {
            (binary(result, B::Max { signed: false }, I8X16, lhs, rhs), 1)
        }
        Op::I16x8MaxS { result, lhs, rhs } => {
            (binary(result, B::Max { signed: true }, I16X8, lhs, rhs), 1)
        }
        Op::I16x8MaxU { result, lhs, rhs } => {
            (binary(result, B::Max { signed: false }, I16X8, lhs, rhs), 1)
        }
        Op::I32x4MaxS { result, lhs, rhs } => {
            (binary(result, B::Max { signed: true }, I32X4, lhs, rhs), 1)
        }
        Op::I32x4MaxU { result, lhs, rhs } => {
            (binary(result, B::Max { signed: false }, I32X4, lhs, rhs), 1)
        }
        Op::I8x16AvgrU { result, lhs, rhs } => (binary(result, B::AvgrU, I8X16, lhs, rhs), 1),
        Op::I16x8AvgrU { result, lhs, rhs } => (binary(result, B::AvgrU, I16X8, lhs, rhs), 1),

        Op::I16x8ExtmulLowI8x16S { result, lhs, rhs } => {
            (extmul(result, I8X16, false, true, lhs, rhs), 1)
        }
        Op::I16x8ExtmulHighI8x16S { result, lhs, rhs } => {
            (extmul(result, I8X16, true, true, lhs, rhs), 1)
        }
        Op::I16x8ExtmulLowI8x16U { result, lhs, rhs } => {
            (extmul(result, I8X16, false, false, lhs, rhs), 1)
        }
        Op::I16x8ExtmulHighI8x16U { result, lhs, rhs } => {
            (extmul(result, I8X16, true, false, lhs, rhs), 1)
        }
        Op::I32x4ExtmulLowI16x8S { result, lhs, rhs } => {
            (extmul(result, I16X8, false, true, lhs, rhs), 1)
        }
        Op::I32x4ExtmulHighI16x8S { result, lhs, rhs } => {
            (extmul(result, I16X8, true, true, lhs, rhs), 1)
        }
        Op::I32x4ExtmulLowI16x8U { result, lhs, rhs } => {
            (extmul(result, I16X8, false, false, lhs, rhs), 1)
        }
        Op::I32x4ExtmulHighI16x8U { result, lhs, rhs } => {
            (extmul(result, I16X8, true, false, lhs, rhs), 1)
        }
        Op::I64x2ExtmulLowI32x4S { result, lhs, rhs } => {
            (extmul(result, I32X4, false, true, lhs, rhs), 1)
        }
        Op::I64x2ExtmulHighI32x4S { result, lhs, rhs } => {
            (extmul(result, I32X4, true, true, lhs, rhs), 1)
        }
        Op::I64x2ExtmulLowI32x4U { result, lhs, rhs } => {
            (extmul(result, I32X4, false, false, lhs, rhs), 1)
        }
        Op::I64x2ExtmulHighI32x4U { result, lhs, rhs } => {
            (extmul(result, I32X4, true, false, lhs, rhs), 1)
        }

        Op::V128And { result, lhs, rhs } => (binary(result, B::And, I8X16, lhs, rhs), 1),
        Op::V128Or { result, lhs, rhs } => (binary(result, B::Or, I8X16, lhs, rhs), 1),
        Op::V128Xor { result, lhs, rhs } => (binary(result, B::Xor, I8X16, lhs, rhs), 1),
        Op::V128Andnot { result, lhs, rhs } => (binary(result, B::AndNot, I8X16, lhs, rhs), 1),

        Op::I8x16Eq { result, lhs, rhs } => (icmp(result, IntCC::Equal, I8X16, lhs, rhs), 1),
        Op::I16x8Eq { result, lhs, rhs } => (icmp(result, IntCC::Equal, I16X8, lhs, rhs), 1),
        Op::I32x4Eq { result, lhs, rhs } => (icmp(result, IntCC::Equal, I32X4, lhs, rhs), 1),
        Op::I64x2Eq { result, lhs, rhs } => (icmp(result, IntCC::Equal, I64X2, lhs, rhs), 1),
        Op::I8x16Ne { result, lhs, rhs } => (icmp(result, IntCC::NotEqual, I8X16, lhs, rhs), 1),
        Op::I16x8Ne { result, lhs, rhs } => (icmp(result, IntCC::NotEqual, I16X8, lhs, rhs), 1),
        Op::I32x4Ne { result, lhs, rhs } => (icmp(result, IntCC::NotEqual, I32X4, lhs, rhs), 1),
        Op::I64x2Ne { result, lhs, rhs } => (icmp(result, IntCC::NotEqual, I64X2, lhs, rhs), 1),
        Op::I8x16LtS { result, lhs, rhs } => {
            (icmp(result, IntCC::SignedLessThan, I8X16, lhs, rhs), 1)
        }
        Op::I8x16LtU { result, lhs, rhs } => {
            (icmp(result, IntCC::UnsignedLessThan, I8X16, lhs, rhs), 1)
        }
        Op::I16x8LtS { result, lhs, rhs } => {
            (icmp(result, IntCC::SignedLessThan, I16X8, lhs, rhs), 1)
        }
        Op::I16x8LtU { result, lhs, rhs } => {
            (icmp(result, IntCC::UnsignedLessThan, I16X8, lhs, rhs), 1)
        }
        Op::I32x4LtS { result, lhs, rhs } => {
            (icmp(result, IntCC::SignedLessThan, I32X4, lhs, rhs), 1)
        }
        Op::I32x4LtU { result, lhs, rhs } => {
            (icmp(result, IntCC::UnsignedLessThan, I32X4, lhs, rhs), 1)
        }
        Op::I64x2LtS { result, lhs, rhs } => {
            (icmp(result, IntCC::SignedLessThan, I64X2, lhs, rhs), 1)
        }
        Op::I8x16LeS { result, lhs, rhs } => (
            icmp(result, IntCC::SignedLessThanOrEqual, I8X16, lhs, rhs),
            1,
        ),
        Op::I8x16LeU { result, lhs, rhs } => (
            icmp(result, IntCC::UnsignedLessThanOrEqual, I8X16, lhs, rhs),
            1,
        ),
        Op::I16x8LeS { result, lhs, rhs } => (
            icmp(result, IntCC::SignedLessThanOrEqual, I16X8, lhs, rhs),
            1,
        ),
        Op::I16x8LeU { result, lhs, rhs } => (
            icmp(result, IntCC::UnsignedLessThanOrEqual, I16X8, lhs, rhs),
            1,
        ),
        Op::I32x4LeS { result, lhs, rhs } => (
            icmp(result, IntCC::SignedLessThanOrEqual, I32X4, lhs, rhs),
            1,
        ),
        Op::I32x4LeU { result, lhs, rhs } => (
            icmp(result, IntCC::UnsignedLessThanOrEqual, I32X4, lhs, rhs),
            1,
        ),
        Op::I64x2LeS { result, lhs, rhs } => (
            icmp(result, IntCC::SignedLessThanOrEqual, I64X2, lhs, rhs),
            1,
        ),
        // Core #op-feq..#op-fle: only `ne` is true for unordered operands.
        Op::F32x4Eq { result, lhs, rhs } => (fcmp(result, FloatCC::Equal, F32X4, lhs, rhs), 1),
        Op::F64x2Eq { result, lhs, rhs } => (fcmp(result, FloatCC::Equal, F64X2, lhs, rhs), 1),
        Op::F32x4Ne { result, lhs, rhs } => (fcmp(result, FloatCC::NotEqual, F32X4, lhs, rhs), 1),
        Op::F64x2Ne { result, lhs, rhs } => (fcmp(result, FloatCC::NotEqual, F64X2, lhs, rhs), 1),
        Op::F32x4Lt { result, lhs, rhs } => (fcmp(result, FloatCC::LessThan, F32X4, lhs, rhs), 1),
        Op::F64x2Lt { result, lhs, rhs } => (fcmp(result, FloatCC::LessThan, F64X2, lhs, rhs), 1),
        Op::F32x4Le { result, lhs, rhs } => {
            (fcmp(result, FloatCC::LessThanOrEqual, F32X4, lhs, rhs), 1)
        }
        Op::F64x2Le { result, lhs, rhs } => {
            (fcmp(result, FloatCC::LessThanOrEqual, F64X2, lhs, rhs), 1)
        }

        Op::F32x4Add { result, lhs, rhs } => (binary(result, B::FAdd, F32X4, lhs, rhs), 1),
        Op::F64x2Add { result, lhs, rhs } => (binary(result, B::FAdd, F64X2, lhs, rhs), 1),
        Op::F32x4Sub { result, lhs, rhs } => (binary(result, B::FSub, F32X4, lhs, rhs), 1),
        Op::F64x2Sub { result, lhs, rhs } => (binary(result, B::FSub, F64X2, lhs, rhs), 1),
        Op::F32x4Mul { result, lhs, rhs } => (binary(result, B::FMul, F32X4, lhs, rhs), 1),
        Op::F64x2Mul { result, lhs, rhs } => (binary(result, B::FMul, F64X2, lhs, rhs), 1),
        Op::F32x4Div { result, lhs, rhs } => (binary(result, B::FDiv, F32X4, lhs, rhs), 1),
        Op::F64x2Div { result, lhs, rhs } => (binary(result, B::FDiv, F64X2, lhs, rhs), 1),
        Op::F32x4Min { result, lhs, rhs } => (binary(result, B::FMin, F32X4, lhs, rhs), 1),
        Op::F64x2Min { result, lhs, rhs } => (binary(result, B::FMin, F64X2, lhs, rhs), 1),
        Op::F32x4Max { result, lhs, rhs } => (binary(result, B::FMax, F32X4, lhs, rhs), 1),
        Op::F64x2Max { result, lhs, rhs } => (binary(result, B::FMax, F64X2, lhs, rhs), 1),
        Op::F32x4Pmin { result, lhs, rhs } => (binary(result, B::FPmin, F32X4, lhs, rhs), 1),
        Op::F64x2Pmin { result, lhs, rhs } => (binary(result, B::FPmin, F64X2, lhs, rhs), 1),
        Op::F32x4Pmax { result, lhs, rhs } => (binary(result, B::FPmax, F32X4, lhs, rhs), 1),
        Op::F64x2Pmax { result, lhs, rhs } => (binary(result, B::FPmax, F64X2, lhs, rhs), 1),

        Op::I8x16NarrowI16x8S { result, lhs, rhs } => (
            binary(result, B::Narrow { signed: true }, I16X8, lhs, rhs),
            1,
        ),
        Op::I8x16NarrowI16x8U { result, lhs, rhs } => (
            binary(result, B::Narrow { signed: false }, I16X8, lhs, rhs),
            1,
        ),
        Op::I16x8NarrowI32x4S { result, lhs, rhs } => (
            binary(result, B::Narrow { signed: true }, I32X4, lhs, rhs),
            1,
        ),
        Op::I16x8NarrowI32x4U { result, lhs, rhs } => (
            binary(result, B::Narrow { signed: false }, I32X4, lhs, rhs),
            1,
        ),

        Op::I8x16Shl { result, lhs, rhs } => {
            (shift(result, Shift::Shl, I8X16, lhs, Operand::Slot(rhs)), 1)
        }
        Op::I16x8Shl { result, lhs, rhs } => {
            (shift(result, Shift::Shl, I16X8, lhs, Operand::Slot(rhs)), 1)
        }
        Op::I32x4Shl { result, lhs, rhs } => {
            (shift(result, Shift::Shl, I32X4, lhs, Operand::Slot(rhs)), 1)
        }
        Op::I64x2Shl { result, lhs, rhs } => {
            (shift(result, Shift::Shl, I64X2, lhs, Operand::Slot(rhs)), 1)
        }
        Op::I8x16ShrS { result, lhs, rhs } => (
            shift(result, Shift::ShrS, I8X16, lhs, Operand::Slot(rhs)),
            1,
        ),
        Op::I16x8ShrS { result, lhs, rhs } => (
            shift(result, Shift::ShrS, I16X8, lhs, Operand::Slot(rhs)),
            1,
        ),
        Op::I32x4ShrS { result, lhs, rhs } => (
            shift(result, Shift::ShrS, I32X4, lhs, Operand::Slot(rhs)),
            1,
        ),
        Op::I64x2ShrS { result, lhs, rhs } => (
            shift(result, Shift::ShrS, I64X2, lhs, Operand::Slot(rhs)),
            1,
        ),
        Op::I8x16ShrU { result, lhs, rhs } => (
            shift(result, Shift::ShrU, I8X16, lhs, Operand::Slot(rhs)),
            1,
        ),
        Op::I16x8ShrU { result, lhs, rhs } => (
            shift(result, Shift::ShrU, I16X8, lhs, Operand::Slot(rhs)),
            1,
        ),
        Op::I32x4ShrU { result, lhs, rhs } => (
            shift(result, Shift::ShrU, I32X4, lhs, Operand::Slot(rhs)),
            1,
        ),
        Op::I64x2ShrU { result, lhs, rhs } => (
            shift(result, Shift::ShrU, I64X2, lhs, Operand::Slot(rhs)),
            1,
        ),
        Op::I8x16ShlBy { result, lhs, rhs } => {
            (shift(result, Shift::Shl, I8X16, lhs, amount(rhs)), 1)
        }
        Op::I16x8ShlBy { result, lhs, rhs } => {
            (shift(result, Shift::Shl, I16X8, lhs, amount(rhs)), 1)
        }
        Op::I32x4ShlBy { result, lhs, rhs } => {
            (shift(result, Shift::Shl, I32X4, lhs, amount(rhs)), 1)
        }
        Op::I64x2ShlBy { result, lhs, rhs } => {
            (shift(result, Shift::Shl, I64X2, lhs, amount(rhs)), 1)
        }
        Op::I8x16ShrSBy { result, lhs, rhs } => {
            (shift(result, Shift::ShrS, I8X16, lhs, amount(rhs)), 1)
        }
        Op::I16x8ShrSBy { result, lhs, rhs } => {
            (shift(result, Shift::ShrS, I16X8, lhs, amount(rhs)), 1)
        }
        Op::I32x4ShrSBy { result, lhs, rhs } => {
            (shift(result, Shift::ShrS, I32X4, lhs, amount(rhs)), 1)
        }
        Op::I64x2ShrSBy { result, lhs, rhs } => {
            (shift(result, Shift::ShrS, I64X2, lhs, amount(rhs)), 1)
        }
        Op::I8x16ShrUBy { result, lhs, rhs } => {
            (shift(result, Shift::ShrU, I8X16, lhs, amount(rhs)), 1)
        }
        Op::I16x8ShrUBy { result, lhs, rhs } => {
            (shift(result, Shift::ShrU, I16X8, lhs, amount(rhs)), 1)
        }
        Op::I32x4ShrUBy { result, lhs, rhs } => {
            (shift(result, Shift::ShrU, I32X4, lhs, amount(rhs)), 1)
        }
        Op::I64x2ShrUBy { result, lhs, rhs } => {
            (shift(result, Shift::ShrU, I64X2, lhs, amount(rhs)), 1)
        }

        Op::V128Not { result, input } => (unary(result, U::Not, I8X16, input), 1),
        Op::I8x16Neg { result, input } => (unary(result, U::Neg, I8X16, input), 1),
        Op::I16x8Neg { result, input } => (unary(result, U::Neg, I16X8, input), 1),
        Op::I32x4Neg { result, input } => (unary(result, U::Neg, I32X4, input), 1),
        Op::I64x2Neg { result, input } => (unary(result, U::Neg, I64X2, input), 1),
        Op::I8x16Abs { result, input } => (unary(result, U::Abs, I8X16, input), 1),
        Op::I16x8Abs { result, input } => (unary(result, U::Abs, I16X8, input), 1),
        Op::I32x4Abs { result, input } => (unary(result, U::Abs, I32X4, input), 1),
        Op::I64x2Abs { result, input } => (unary(result, U::Abs, I64X2, input), 1),
        Op::I8x16Popcnt { result, input } => (unary(result, U::Popcnt, I8X16, input), 1),
        Op::F32x4Neg { result, input } => (unary(result, U::FNeg, F32X4, input), 1),
        Op::F64x2Neg { result, input } => (unary(result, U::FNeg, F64X2, input), 1),
        Op::F32x4Abs { result, input } => (unary(result, U::FAbs, F32X4, input), 1),
        Op::F64x2Abs { result, input } => (unary(result, U::FAbs, F64X2, input), 1),
        Op::F32x4Sqrt { result, input } => (unary(result, U::Sqrt, F32X4, input), 1),
        Op::F64x2Sqrt { result, input } => (unary(result, U::Sqrt, F64X2, input), 1),
        Op::F32x4Ceil { result, input } => (unary(result, U::Ceil, F32X4, input), 1),
        Op::F64x2Ceil { result, input } => (unary(result, U::Ceil, F64X2, input), 1),
        Op::F32x4Floor { result, input } => (unary(result, U::Floor, F32X4, input), 1),
        Op::F64x2Floor { result, input } => (unary(result, U::Floor, F64X2, input), 1),
        Op::F32x4Trunc { result, input } => (unary(result, U::Trunc, F32X4, input), 1),
        Op::F64x2Trunc { result, input } => (unary(result, U::Trunc, F64X2, input), 1),
        Op::F32x4Nearest { result, input } => (unary(result, U::Nearest, F32X4, input), 1),
        Op::F64x2Nearest { result, input } => (unary(result, U::Nearest, F64X2, input), 1),

        Op::I16x8ExtendLowI8x16S { result, input } => {
            (extend(result, I8X16, false, true, input), 1)
        }
        Op::I16x8ExtendHighI8x16S { result, input } => {
            (extend(result, I8X16, true, true, input), 1)
        }
        Op::I16x8ExtendLowI8x16U { result, input } => {
            (extend(result, I8X16, false, false, input), 1)
        }
        Op::I16x8ExtendHighI8x16U { result, input } => {
            (extend(result, I8X16, true, false, input), 1)
        }
        Op::I32x4ExtendLowI16x8S { result, input } => {
            (extend(result, I16X8, false, true, input), 1)
        }
        Op::I32x4ExtendHighI16x8S { result, input } => {
            (extend(result, I16X8, true, true, input), 1)
        }
        Op::I32x4ExtendLowI16x8U { result, input } => {
            (extend(result, I16X8, false, false, input), 1)
        }
        Op::I32x4ExtendHighI16x8U { result, input } => {
            (extend(result, I16X8, true, false, input), 1)
        }
        Op::I64x2ExtendLowI32x4S { result, input } => {
            (extend(result, I32X4, false, true, input), 1)
        }
        Op::I64x2ExtendHighI32x4S { result, input } => {
            (extend(result, I32X4, true, true, input), 1)
        }
        Op::I64x2ExtendLowI32x4U { result, input } => {
            (extend(result, I32X4, false, false, input), 1)
        }
        Op::I64x2ExtendHighI32x4U { result, input } => {
            (extend(result, I32X4, true, false, input), 1)
        }
        Op::I16x8ExtaddPairwiseI8x16S { result, input } => (
            unary(result, U::ExtAddPairwise { signed: true }, I8X16, input),
            1,
        ),
        Op::I16x8ExtaddPairwiseI8x16U { result, input } => (
            unary(result, U::ExtAddPairwise { signed: false }, I8X16, input),
            1,
        ),
        Op::I32x4ExtaddPairwiseI16x8S { result, input } => (
            unary(result, U::ExtAddPairwise { signed: true }, I16X8, input),
            1,
        ),
        Op::I32x4ExtaddPairwiseI16x8U { result, input } => (
            unary(result, U::ExtAddPairwise { signed: false }, I16X8, input),
            1,
        ),

        Op::F32x4ConvertI32x4S { result, input } => (
            unary(result, U::ConvertI32 { signed: true }, I32X4, input),
            1,
        ),
        Op::F32x4ConvertI32x4U { result, input } => (
            unary(result, U::ConvertI32 { signed: false }, I32X4, input),
            1,
        ),
        Op::F64x2ConvertLowI32x4S { result, input } => (
            unary(result, U::ConvertLowI32 { signed: true }, I32X4, input),
            1,
        ),
        Op::F64x2ConvertLowI32x4U { result, input } => (
            unary(result, U::ConvertLowI32 { signed: false }, I32X4, input),
            1,
        ),
        Op::I32x4TruncSatF32x4S { result, input } => (
            unary(result, U::TruncSatF32 { signed: true }, F32X4, input),
            1,
        ),
        Op::I32x4TruncSatF32x4U { result, input } => (
            unary(result, U::TruncSatF32 { signed: false }, F32X4, input),
            1,
        ),
        Op::I32x4TruncSatF64x2SZero { result, input } => (
            unary(result, U::TruncSatF64Zero { signed: true }, F64X2, input),
            1,
        ),
        Op::I32x4TruncSatF64x2UZero { result, input } => (
            unary(result, U::TruncSatF64Zero { signed: false }, F64X2, input),
            1,
        ),
        Op::F32x4DemoteF64x2Zero { result, input } => (unary(result, U::Demote, F64X2, input), 1),
        Op::F64x2PromoteLowF32x4 { result, input } => (unary(result, U::Promote, F32X4, input), 1),

        Op::V128AnyTrue { result, input } => (test(result, Test::AnyTrue, I8X16, input), 1),
        Op::I8x16AllTrue { result, input } => (test(result, Test::AllTrue, I8X16, input), 1),
        Op::I16x8AllTrue { result, input } => (test(result, Test::AllTrue, I16X8, input), 1),
        Op::I32x4AllTrue { result, input } => (test(result, Test::AllTrue, I32X4, input), 1),
        Op::I64x2AllTrue { result, input } => (test(result, Test::AllTrue, I64X2, input), 1),
        Op::I8x16Bitmask { result, input } => (test(result, Test::Bitmask, I8X16, input), 1),
        Op::I16x8Bitmask { result, input } => (test(result, Test::Bitmask, I16X8, input), 1),
        Op::I32x4Bitmask { result, input } => (test(result, Test::Bitmask, I32X4, input), 1),
        Op::I64x2Bitmask { result, input } => (test(result, Test::Bitmask, I64X2, input), 1),
        _ => return None,
    };
    Some((NativeOp::Vector(vector), words))
}

fn amount(rhs: crate::ir::ShiftAmount<u32>) -> Operand {
    Operand::Constant(i64::from(u32::from(rhs)))
}

fn icmp(result: Slot, cc: IntCC, ty: Type, lhs: Slot, rhs: Slot) -> VectorOp {
    binary(result, Binary::Icmp(cc), ty, lhs, rhs)
}

fn fcmp(result: Slot, cc: FloatCC, ty: Type, lhs: Slot, rhs: Slot) -> VectorOp {
    binary(result, Binary::Fcmp(cc), ty, lhs, rhs)
}

fn extmul(result: Slot, ty: Type, high: bool, signed: bool, lhs: Slot, rhs: Slot) -> VectorOp {
    binary(result, Binary::ExtMul { high, signed }, ty, lhs, rhs)
}

fn extend(result: Slot, ty: Type, high: bool, signed: bool, input: Slot) -> VectorOp {
    unary(result, Unary::Extend { high, signed }, ty, input)
}

fn extend_load(lane: Type, signed: bool) -> Load {
    Load::Extend { lane, signed }
}

fn splat(result: Slot, ty: Type, value: Slot) -> (VectorOp, usize) {
    (VectorOp::Splat { result, ty, value }, 1)
}

fn extract(result: Slot, input: Slot, ty: Type, lane: u8, signed: bool) -> (VectorOp, usize) {
    (
        VectorOp::Extract {
            result,
            ty,
            lane,
            input,
            signed,
        },
        1,
    )
}

fn replace_slot(
    result: Slot,
    input: Slot,
    ty: Type,
    lane: u8,
    param: Option<Op>,
) -> Option<(VectorOp, usize)> {
    let Some(Op::Slot { slot }) = param else {
        return None;
    };
    Some((
        VectorOp::Replace {
            result,
            ty,
            lane,
            input,
            value: Operand::Slot(slot),
        },
        2,
    ))
}

fn replace_constant(
    result: Slot,
    input: Slot,
    ty: Type,
    lane: u8,
    value: i64,
) -> (VectorOp, usize) {
    (
        VectorOp::Replace {
            result,
            ty,
            lane,
            input,
            value: Operand::Constant(value),
        },
        2,
    )
}

fn general_load(
    result: Slot,
    offset_lo: crate::ir::Offset64Lo,
    pointer: Option<Op>,
    kind: Load,
) -> Option<(VectorOp, usize)> {
    let (ptr, offset) = split_offset(offset_lo, pointer)?;
    Some((
        VectorOp::Load {
            result,
            ptr: Operand::Slot(ptr),
            offset,
            kind,
        },
        2,
    ))
}

/// The `input` vector and lane of a `v128.loadN_lane` operand word.
fn input_lane(ty: Type, word: Option<Op>) -> Option<(Slot, u8)> {
    let Some(Op::SlotAndImm32 { slot, imm }) = word else {
        return None;
    };
    let lane = u32::from(imm);
    (lane < 16 / ty.bytes()).then_some((slot, lane as u8))
}

fn load_lane(
    result: Slot,
    ty: Type,
    offset_lo: crate::ir::Offset64Lo,
    pointer: Option<Op>,
    lane_word: Option<Op>,
) -> Option<(VectorOp, usize)> {
    let (ptr, offset) = split_offset(offset_lo, pointer)?;
    let (input, lane) = input_lane(ty, lane_word)?;
    Some((
        VectorOp::Load {
            result,
            ptr: Operand::Slot(ptr),
            offset,
            kind: Load::Lane { ty, lane, input },
        },
        3,
    ))
}

fn load_lane_at(
    result: Slot,
    ty: Type,
    address: crate::ir::Address32,
    lane_word: Option<Op>,
) -> Option<(VectorOp, usize)> {
    let (input, lane) = input_lane(ty, lane_word)?;
    Some((
        load_constant(result, address, Load::Lane { ty, lane, input }),
        2,
    ))
}

/// The lane of a `v128.storeN_lane` operand word, if it names memory 0.
fn store_lane_index(ty: Type, word: Option<Op>) -> Option<u8> {
    let Some(Op::Imm16AndImm32 { imm16, imm32 }) = word else {
        return None;
    };
    let lane = i16::from(imm16) as u16;
    (u32::from(imm32) == 0 && u32::from(lane) < 16 / ty.bytes()).then_some(lane as u8)
}

fn store_lane_general(
    ptr: Slot,
    offset_lo: crate::ir::Offset64Lo,
    ty: Type,
    value_word: Option<Op>,
    lane_word: Option<Op>,
) -> Option<(VectorOp, usize)> {
    let (value, offset) = split_offset(offset_lo, value_word)?;
    let lane = store_lane_index(ty, lane_word)?;
    Some((
        VectorOp::Store {
            value,
            ptr: Operand::Slot(ptr),
            offset,
            kind: Store::Lane { ty, lane },
        },
        3,
    ))
}

fn store_lane_at(
    value: Slot,
    address: crate::ir::Address32,
    ty: Type,
    lane_word: Option<Op>,
) -> Option<(VectorOp, usize)> {
    let lane = store_lane_index(ty, lane_word)?;
    Some((
        VectorOp::Store {
            value,
            ptr: Operand::Constant(usize::from(address) as i64),
            offset: 0,
            kind: Store::Lane { ty, lane },
        },
        2,
    ))
}

fn store_lane(
    ptr: Slot,
    value: Slot,
    offset: crate::ir::Offset8,
    ty: Type,
    lane: u8,
) -> (VectorOp, usize) {
    (
        VectorOp::Store {
            value,
            ptr: Operand::Slot(ptr),
            offset: u64::from(Offset64::from(offset)) as u32,
            kind: Store::Lane { ty, lane },
        },
        1,
    )
}

/// The vector type with lanes of scalar type `lane`.
fn vector_of(lane: Type) -> Type {
    lane.by(16 / lane.bytes())
        .expect("128-bit vector lane type")
}

fn widen(b: &mut FunctionBuilder<'_>, value: Value, high: bool, signed: bool) -> Value {
    match (high, signed) {
        (false, true) => b.ins().swiden_low(value),
        (true, true) => b.ins().swiden_high(value),
        (false, false) => b.ins().uwiden_low(value),
        (true, false) => b.ins().uwiden_high(value),
    }
}

/// A scalar operand as a value of `lane` type, reinterpreting float bits.
fn lane_value(b: &mut FunctionBuilder<'_>, slots: &Slots, lane: Type, operand: Operand) -> Value {
    match lane {
        F32 => {
            let bits = slots.read(b, I32, operand);
            b.ins().bitcast(F32, MemFlags::new(), bits)
        }
        F64 => {
            let bits = slots.read(b, I64, operand);
            b.ins().bitcast(F64, MemFlags::new(), bits)
        }
        _ => slots.read(b, lane, operand),
    }
}

fn emit_binary(
    b: &mut FunctionBuilder<'_>,
    slots: &Slots,
    kind: Binary,
    ty: Type,
    lhs: Slot,
    rhs: Slot,
) -> Value {
    let x = slots.vector(b, lhs, ty);
    let y = slots.vector(b, rhs, ty);
    match kind {
        Binary::Add => b.ins().iadd(x, y),
        Binary::Sub => b.ins().isub(x, y),
        Binary::Mul => b.ins().imul(x, y),
        Binary::AddSat { signed: true } => b.ins().sadd_sat(x, y),
        Binary::AddSat { signed: false } => b.ins().uadd_sat(x, y),
        Binary::SubSat { signed: true } => b.ins().ssub_sat(x, y),
        Binary::SubSat { signed: false } => b.ins().usub_sat(x, y),
        Binary::Min { signed: true } => b.ins().smin(x, y),
        Binary::Min { signed: false } => b.ins().umin(x, y),
        Binary::Max { signed: true } => b.ins().smax(x, y),
        Binary::Max { signed: false } => b.ins().umax(x, y),
        Binary::AvgrU => b.ins().avg_round(x, y),
        Binary::Q15MulrSatS => b.ins().sqmul_round_sat(x, y),
        Binary::And => b.ins().band(x, y),
        Binary::Or => b.ins().bor(x, y),
        Binary::Xor => b.ins().bxor(x, y),
        Binary::AndNot => {
            let not = b.ins().bnot(y);
            b.ins().band(x, not)
        }
        // Core #op-iswizzle-lane: selector lanes >= 16 produce 0, exactly
        // Cranelift's `swizzle`.
        Binary::Swizzle => b.ins().swizzle(x, y),
        Binary::Icmp(cc) => b.ins().icmp(cc, x, y),
        Binary::Fcmp(cc) => b.ins().fcmp(cc, x, y),
        Binary::FAdd => b.ins().fadd(x, y),
        Binary::FSub => b.ins().fsub(x, y),
        Binary::FMul => b.ins().fmul(x, y),
        Binary::FDiv => b.ins().fdiv(x, y),
        // Cranelift's `fmin`/`fmax` implement Core #op-fmin/#op-fmax.
        Binary::FMin => b.ins().fmin(x, y),
        Binary::FMax => b.ins().fmax(x, y),
        // Core #op-fpmin: pmin(z1, z2) = z2 if z2 < z1, else z1.
        Binary::FPmin => {
            let less = b.ins().fcmp(FloatCC::LessThan, y, x);
            let less = super::cast(b, ty, less);
            b.ins().bitselect(less, y, x)
        }
        // Core #op-fpmax: pmax(z1, z2) = z2 if z1 < z2, else z1.
        Binary::FPmax => {
            let less = b.ins().fcmp(FloatCC::LessThan, x, y);
            let less = super::cast(b, ty, less);
            b.ins().bitselect(less, y, x)
        }
        // `x` supplies the low result lanes and `y` the high ones; Cranelift's
        // `unarrow` treats its inputs as signed, as Core #op-narrow requires.
        Binary::Narrow { signed: true } => b.ins().snarrow(x, y),
        Binary::Narrow { signed: false } => b.ins().unarrow(x, y),
        Binary::ExtMul { high, signed } => {
            let x = widen(b, x, high, signed);
            let y = widen(b, y, high, signed);
            b.ins().imul(x, y)
        }
        // Core #exec-vextbinop (DOT): lane i of the result is the sum of the
        // sign-extended products of input lanes 2i and 2i+1.
        Binary::Dot => {
            let x_low = b.ins().swiden_low(x);
            let y_low = b.ins().swiden_low(y);
            let low = b.ins().imul(x_low, y_low);
            let x_high = b.ins().swiden_high(x);
            let y_high = b.ins().swiden_high(y);
            let high = b.ins().imul(x_high, y_high);
            b.ins().iadd_pairwise(low, high)
        }
    }
}

fn emit_unary(
    b: &mut FunctionBuilder<'_>,
    slots: &Slots,
    kind: Unary,
    ty: Type,
    input: Slot,
) -> Value {
    let x = slots.vector(b, input, ty);
    match kind {
        Unary::Not => b.ins().bnot(x),
        Unary::Neg => b.ins().ineg(x),
        Unary::Abs => b.ins().iabs(x),
        Unary::Popcnt => b.ins().popcnt(x),
        Unary::FNeg => b.ins().fneg(x),
        Unary::FAbs => b.ins().fabs(x),
        Unary::Sqrt => b.ins().sqrt(x),
        Unary::Ceil => b.ins().ceil(x),
        Unary::Floor => b.ins().floor(x),
        Unary::Trunc => b.ins().trunc(x),
        // Ties to even, as Core #op-fnearest requires.
        Unary::Nearest => b.ins().nearest(x),
        Unary::Extend { high, signed } => widen(b, x, high, signed),
        Unary::ExtAddPairwise { signed } => {
            let low = widen(b, x, false, signed);
            let high = widen(b, x, true, signed);
            b.ins().iadd_pairwise(low, high)
        }
        Unary::ConvertI32 { signed: true } => b.ins().fcvt_from_sint(F32X4, x),
        Unary::ConvertI32 { signed: false } => b.ins().fcvt_from_uint(F32X4, x),
        Unary::ConvertLowI32 { signed } => {
            let wide = widen(b, x, false, signed);
            if signed {
                b.ins().fcvt_from_sint(F64X2, wide)
            } else {
                b.ins().fcvt_from_uint(F64X2, wide)
            }
        }
        // Saturating conversions map NaN to 0 (Core #op-trunc_sat_u/s).
        Unary::TruncSatF32 { signed: true } => b.ins().fcvt_to_sint_sat(I32X4, x),
        Unary::TruncSatF32 { signed: false } => b.ins().fcvt_to_uint_sat(I32X4, x),
        Unary::TruncSatF64Zero { signed } => {
            let wide = if signed {
                b.ins().fcvt_to_sint_sat(I64X2, x)
            } else {
                b.ins().fcvt_to_uint_sat(I64X2, x)
            };
            let zero = zero_vector(b, I64X2);
            if signed {
                b.ins().snarrow(wide, zero)
            } else {
                b.ins().uunarrow(wide, zero)
            }
        }
        Unary::Demote => b.ins().fvdemote(x),
        Unary::Promote => b.ins().fvpromote_low(x),
    }
}

fn zero_vector(b: &mut FunctionBuilder<'_>, ty: Type) -> Value {
    let constant = b
        .func
        .dfg
        .constants
        .insert(ConstantData::from(&[0_u8; 16][..]));
    b.ins().vconst(ty, constant)
}

/// Emits one decoded vector instruction. Memory accesses leave the region
/// through `exits` at `instruction` when their bounds check fails, so the
/// interpreter raises the trap exactly as before.
pub(super) fn emit(
    b: &mut FunctionBuilder<'_>,
    slots: &Slots,
    memory: &MemoryCode,
    exits: &mut BTreeMap<isize, Block>,
    instruction: usize,
    op: VectorOp,
) {
    match op {
        VectorOp::Binary {
            result,
            kind,
            ty,
            lhs,
            rhs,
        } => {
            let value = emit_binary(b, slots, kind, ty, lhs, rhs);
            slots.set_vector(b, result, value);
        }
        VectorOp::Unary {
            result,
            kind,
            ty,
            input,
        } => {
            let value = emit_unary(b, slots, kind, ty, input);
            slots.set_vector(b, result, value);
        }
        // Core #op-ibitselect: (lhs AND mask) OR (rhs AND NOT mask).
        VectorOp::Bitselect {
            result,
            lhs,
            rhs,
            mask,
        } => {
            let x = slots.vector(b, lhs, I8X16);
            let y = slots.vector(b, rhs, I8X16);
            let mask = slots.vector(b, mask, I8X16);
            let value = b.ins().bitselect(mask, x, y);
            slots.set_vector(b, result, value);
        }
        // Core #exec-vshuffle: lanes 0..15 select from `lhs`, 16..31 from `rhs`,
        // which is Cranelift's immediate `shuffle` mask.
        VectorOp::Shuffle {
            result,
            lhs,
            rhs,
            lanes,
        } => {
            let x = slots.vector(b, lhs, I8X16);
            let y = slots.vector(b, rhs, I8X16);
            let mask = b.func.dfg.immediates.push(ConstantData::from(&lanes[..]));
            let value = b.ins().shuffle(x, y, mask);
            slots.set_vector(b, result, value);
        }
        VectorOp::Shift {
            result,
            kind,
            ty,
            lhs,
            amount,
        } => {
            let x = slots.vector(b, lhs, ty);
            let amount = slots.read(b, I32, amount);
            let value = match kind {
                Shift::Shl => b.ins().ishl(x, amount),
                Shift::ShrS => b.ins().sshr(x, amount),
                Shift::ShrU => b.ins().ushr(x, amount),
            };
            slots.set_vector(b, result, value);
        }
        VectorOp::Splat { result, ty, value } => {
            let lane = lane_value(b, slots, ty.lane_type(), Operand::Slot(value));
            let value = b.ins().splat(ty, lane);
            slots.set_vector(b, result, value);
        }
        VectorOp::Extract {
            result,
            ty,
            lane,
            input,
            signed,
        } => {
            let x = slots.vector(b, input, ty);
            let value = b.ins().extractlane(x, lane);
            let value = match ty.lane_type() {
                I8 | I16 if signed => b.ins().sextend(I32, value),
                I8 | I16 => b.ins().uextend(I32, value),
                F32 => b.ins().bitcast(I32, MemFlags::new(), value),
                F64 => b.ins().bitcast(I64, MemFlags::new(), value),
                _ => value,
            };
            slots.write(b, result, value);
        }
        VectorOp::Replace {
            result,
            ty,
            lane,
            input,
            value,
        } => {
            let x = slots.vector(b, input, ty);
            let scalar = lane_value(b, slots, ty.lane_type(), value);
            let value = b.ins().insertlane(x, scalar, lane);
            slots.set_vector(b, result, value);
        }
        VectorOp::Test {
            result,
            kind,
            ty,
            input,
        } => {
            let x = slots.vector(b, input, ty);
            let value = match kind {
                Test::AnyTrue => b.ins().vany_true(x),
                Test::AllTrue => b.ins().vall_true(x),
                // Core #exec-vbitmask: bit i is the sign bit of lane i.
                Test::Bitmask => b.ins().vhigh_bits(I32, x),
            };
            slots.write(b, result, value);
        }
        VectorOp::Load {
            result,
            ptr,
            offset,
            kind,
        } => {
            let width = match kind {
                Load::Full => I8X16,
                Load::Splat(lane) | Load::Zero(lane) | Load::Lane { ty: lane, .. } => lane,
                Load::Extend { .. } => I64,
            };
            let (address, _) = memory.checked_address(
                b,
                slots,
                exits,
                instruction,
                MemoryAccess { ptr, offset, width },
            );
            let flags = MemFlags::new().with_notrap();
            let value = match kind {
                Load::Full => b.ins().load(I8X16, flags, address, 0),
                Load::Splat(lane) => {
                    let scalar = b.ins().load(lane, flags, address, 0);
                    b.ins().splat(vector_of(lane), scalar)
                }
                Load::Zero(lane) => {
                    let scalar = b.ins().load(lane, flags, address, 0);
                    b.ins().scalar_to_vector(vector_of(lane), scalar)
                }
                Load::Extend { lane, signed } => match (lane, signed) {
                    (I8, true) => b.ins().sload8x8(flags, address, 0),
                    (I8, false) => b.ins().uload8x8(flags, address, 0),
                    (I16, true) => b.ins().sload16x4(flags, address, 0),
                    (I16, false) => b.ins().uload16x4(flags, address, 0),
                    (_, true) => b.ins().sload32x2(flags, address, 0),
                    (_, false) => b.ins().uload32x2(flags, address, 0),
                },
                Load::Lane { ty, lane, input } => {
                    let scalar = b.ins().load(ty, flags, address, 0);
                    let x = slots.vector(b, input, vector_of(ty));
                    b.ins().insertlane(x, scalar, lane)
                }
            };
            slots.set_vector(b, result, value);
        }
        VectorOp::Store {
            value,
            ptr,
            offset,
            kind,
        } => {
            let width = match kind {
                Store::Full => I8X16,
                Store::Lane { ty, .. } => ty,
            };
            let (address, relative) = memory.checked_address(
                b,
                slots,
                exits,
                instruction,
                MemoryAccess { ptr, offset, width },
            );
            let stored = match kind {
                Store::Full => slots.vector(b, value, I8X16),
                Store::Lane { ty, lane } => {
                    let x = slots.vector(b, value, vector_of(ty));
                    b.ins().extractlane(x, lane)
                }
            };
            b.ins()
                .store(MemFlags::new().with_notrap(), stored, address, 0);
            memory.mark_dirty(b, relative, width);
        }
    }
}
