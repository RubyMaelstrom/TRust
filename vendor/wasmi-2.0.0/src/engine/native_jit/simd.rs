//! Native lowering of WebAssembly 128-bit vector operators.
//!
//! WebAssembly Core (local official snapshot 37d6b059, 2026-09-06):
//! #syntax-instr-vec, #exec-instr-vec and the vector numerics of
//! `specification/wasm-latest/3.2-numerics.vector.spectec`.
//!
//! * Lanes are numbered from the least significant byte of the little-endian
//!   v128. Two frame cells (low cell first) and linear memory both store lane
//!   `i` of an `i8x16` at byte `i`, so 16-byte loads and stores need no lane
//!   reordering on the little-endian hosts that run native code.
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
//! Wasmi translates the extending, splatting, zeroing and lane loads and the
//! 32- and 64-bit lane stores to scalar memory operators combined with the
//! `widen`, `splat`, `low_zero`, `replace_lane` and `extract_lane` operators
//! lowered here. Relaxed SIMD and accesses of non-default memories or with
//! offsets above `u32::MAX` remain interpreter boundaries.

use super::{Dest, MemoryAccess, MemoryCode, Operand, Reg, Slots, unaligned};
use crate::ir::GlobalAddr;
use cranelift_codegen::ir::{
    Block,
    ConstantData,
    InstBuilder,
    MemFlagsData as MemFlags,
    Type,
    Value,
    condcodes::{FloatCC, IntCC},
    types::{F32X4, F64X2, I8, I8X16, I16, I32, I32X4, I64, I64X2},
};
use cranelift_frontend::FunctionBuilder;
use std::collections::BTreeMap;

/// A lane-wise operation on two vectors of lane type `ty`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Binary {
    Add,
    Sub,
    Mul,
    AddSat { signed: bool },
    SubSat { signed: bool },
    Min { signed: bool },
    Max { signed: bool },
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
    Narrow { signed: bool },
    /// `ty` is the narrower source lane type.
    ExtMul { high: bool, signed: bool },
    /// `i32x4.dot_i16x8_s`; `ty` is `i16x8`.
    Dot,
}

/// A lane-wise operation on one vector of lane type `ty`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Unary {
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
    Extend { high: bool, signed: bool },
    /// `ty` is the narrower source lane type.
    ExtAddPairwise { signed: bool },
    /// `f32x4.convert_i32x4_{s,u}`.
    ConvertI32 { signed: bool },
    /// `f64x2.convert_low_i32x4_{s,u}`.
    ConvertLowI32 { signed: bool },
    /// `i32x4.trunc_sat_f32x4_{s,u}`.
    TruncSatF32 { signed: bool },
    /// `i32x4.trunc_sat_f64x2_{s,u}_zero`.
    TruncSatF64Zero { signed: bool },
    /// `f32x4.demote_f64x2_zero`.
    Demote,
    /// `f64x2.promote_low_f32x4`.
    Promote,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Shift {
    Shl,
    ShrS,
    ShrU,
}

/// A vector-to-`i32` reduction.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Test {
    AnyTrue,
    AllTrue,
    Bitmask,
}

/// A v128 operator; `u32` operands are the first of the two cells of a v128.
#[derive(Clone, Copy, Debug)]
pub(crate) enum VectorOp {
    Binary {
        result: u32,
        kind: Binary,
        ty: Type,
        lhs: u32,
        rhs: u32,
    },
    Unary {
        result: u32,
        kind: Unary,
        ty: Type,
        input: u32,
    },
    /// `v128.bitselect(lhs, rhs, mask)`.
    Bitselect {
        result: u32,
        lhs: u32,
        rhs: u32,
        mask: u32,
    },
    Shuffle {
        result: u32,
        lhs: u32,
        rhs: u32,
        lanes: [u8; 16],
    },
    Shift {
        result: u32,
        kind: Shift,
        ty: Type,
        lhs: u32,
        amount: Operand,
    },
    /// Splats the low lane bits of `value` into a vector of type `ty`.
    Splat {
        result: u32,
        ty: Type,
        value: Operand,
    },
    /// Extracts `lane` of `input`; `signed` selects `extract_lane_s` for
    /// 8- and 16-bit lanes. Float lanes are extracted as their bits.
    Extract {
        result: Dest,
        ty: Type,
        lane: u8,
        input: u32,
        signed: bool,
    },
    Replace {
        result: u32,
        ty: Type,
        lane: u8,
        input: u32,
        value: Operand,
    },
    Test {
        result: Dest,
        kind: Test,
        ty: Type,
        input: u32,
    },
    /// Widens the eight bytes of the 64-bit `value` (as the low half of an
    /// input of the narrower lane type) to a vector of type `ty`.
    Widen {
        result: u32,
        ty: Type,
        signed: bool,
        value: Operand,
    },
    /// The vector whose low `lane` holds `value` and whose other lanes are 0.
    LowZero {
        result: u32,
        lane: Type,
        value: Operand,
    },
    Copy {
        result: u32,
        input: u32,
    },
    Constant {
        result: u32,
        bytes: [u8; 16],
    },
    /// Core #exec-select: selects the complete v128.
    Select {
        result: u32,
        condition: Operand,
        values: [u32; 2],
    },
    /// `v128.load`.
    Load {
        result: u32,
        ptr: Operand,
        offset: u32,
    },
    /// `v128.store`, or `v128.storeN_lane` with `(lane type, lane)`.
    Store {
        value: u32,
        ptr: Operand,
        offset: u32,
        lane: Option<(Type, u8)>,
    },
    GlobalGet {
        result: u32,
        global: GlobalAddr,
    },
    GlobalSet {
        value: u32,
        global: GlobalAddr,
    },
}

impl VectorOp {
    /// Returns the global accessed by `self` if any.
    pub(super) fn global(&self) -> Option<GlobalAddr> {
        match *self {
            VectorOp::GlobalGet { global, .. } | VectorOp::GlobalSet { global, .. } => Some(global),
            _ => None,
        }
    }

    /// Visits `(cell, written, holds_v128)` for every frame cell accessed and
    /// `(register, written)` for every accumulator register accessed.
    pub(super) fn visit(
        &self,
        cells: &mut dyn FnMut(u32, bool, bool),
        regs: &mut dyn FnMut(Reg, bool),
    ) {
        match *self {
            VectorOp::Binary {
                result, lhs, rhs, ..
            }
            | VectorOp::Shuffle {
                result, lhs, rhs, ..
            } => {
                cells(lhs, false, true);
                cells(rhs, false, true);
                cells(result, true, true);
            }
            VectorOp::Unary { result, input, .. } | VectorOp::Copy { result, input } => {
                cells(input, false, true);
                cells(result, true, true);
            }
            VectorOp::Bitselect {
                result,
                lhs,
                rhs,
                mask,
            } => {
                cells(lhs, false, true);
                cells(rhs, false, true);
                cells(mask, false, true);
                cells(result, true, true);
            }
            VectorOp::Shift {
                result,
                lhs,
                amount,
                ..
            } => {
                cells(lhs, false, true);
                cells(result, true, true);
                amount.visit(cells, regs);
            }
            VectorOp::Splat { result, value, .. }
            | VectorOp::Widen { result, value, .. }
            | VectorOp::LowZero { result, value, .. } => {
                cells(result, true, true);
                value.visit(cells, regs);
            }
            VectorOp::Extract { result, input, .. } | VectorOp::Test { result, input, .. } => {
                cells(input, false, true);
                result.visit(cells, regs);
            }
            VectorOp::Replace {
                result,
                input,
                value,
                ..
            } => {
                cells(input, false, true);
                cells(result, true, true);
                value.visit(cells, regs);
            }
            VectorOp::Constant { result, .. } | VectorOp::GlobalGet { result, .. } => {
                cells(result, true, true);
            }
            VectorOp::Select {
                result,
                condition,
                values,
            } => {
                cells(values[0], false, true);
                cells(values[1], false, true);
                cells(result, true, true);
                condition.visit(cells, regs);
            }
            VectorOp::Load { result, ptr, .. } => {
                cells(result, true, true);
                ptr.visit(cells, regs);
            }
            VectorOp::Store { value, ptr, .. } => {
                cells(value, false, true);
                ptr.visit(cells, regs);
            }
            VectorOp::GlobalSet { value, .. } => cells(value, false, true),
        }
    }
}

/// The vector type with lanes of scalar type `lane`.
pub(super) fn vector_of(lane: Type) -> Type {
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

fn emit_binary(
    b: &mut FunctionBuilder<'_>,
    slots: &Slots,
    kind: Binary,
    ty: Type,
    lhs: u32,
    rhs: u32,
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
    input: u32,
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

fn vector_constant(b: &mut FunctionBuilder<'_>, ty: Type, bytes: &[u8; 16]) -> Value {
    let constant = b
        .func
        .dfg
        .constants
        .insert(ConstantData::from(&bytes[..]));
    b.ins().vconst(ty, constant)
}

fn zero_vector(b: &mut FunctionBuilder<'_>, ty: Type) -> Value {
    vector_constant(b, ty, &[0_u8; 16])
}

/// Emits one decoded vector operator at `operator`. Memory accesses leave the
/// region through `exits` at `operator` when their bounds check fails, so the
/// interpreter raises the trap exactly as before. `global` is the address of
/// the value of the global accessed by `op`, if any.
pub(super) fn emit(
    b: &mut FunctionBuilder<'_>,
    slots: &Slots,
    memory: &MemoryCode,
    exits: &mut BTreeMap<usize, Block>,
    operator: usize,
    global: Option<Value>,
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
            let lane = slots.read(b, ty.lane_type(), value);
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
            let scalar = slots.read(b, ty.lane_type(), value);
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
        VectorOp::Widen {
            result,
            ty,
            signed,
            value,
        } => {
            // The eight loaded bytes form the low half of a vector of the
            // narrower source lanes (Core #exec-vload, `vec_ext`).
            let scalar = slots.read(b, I64, value);
            let x = b.ins().scalar_to_vector(I64X2, scalar);
            let narrow = ty.lane_type().half_width().expect("widened lane type");
            let x = super::cast(b, vector_of(narrow), x);
            let value = widen(b, x, false, signed);
            slots.set_vector(b, result, value);
        }
        VectorOp::LowZero {
            result,
            lane,
            value,
        } => {
            let scalar = slots.read(b, lane, value);
            let value = b.ins().scalar_to_vector(vector_of(lane), scalar);
            slots.set_vector(b, result, value);
        }
        VectorOp::Copy { result, input } => {
            let value = slots.vector(b, input, I8X16);
            slots.set_vector(b, result, value);
        }
        VectorOp::Constant { result, bytes } => {
            let value = vector_constant(b, I8X16, &bytes);
            slots.set_vector(b, result, value);
        }
        VectorOp::Select {
            result,
            condition,
            values,
        } => {
            let condition = slots.read(b, I32, condition);
            let yes = slots.vector(b, values[0], I8X16);
            let no = slots.vector(b, values[1], I8X16);
            let value = b.ins().select(condition, yes, no);
            slots.set_vector(b, result, value);
        }
        VectorOp::GlobalGet { result, .. } => {
            let global = global.expect("vector global address");
            let value = b.ins().load(I8X16, unaligned(), global, 0);
            slots.set_vector(b, result, value);
        }
        VectorOp::GlobalSet { value, .. } => {
            let global = global.expect("vector global address");
            let value = slots.vector(b, value, I8X16);
            b.ins().store(unaligned(), value, global, 0);
        }
        VectorOp::Load {
            result,
            ptr,
            offset,
        } => {
            let (address, _) = memory.checked_address(
                b,
                slots,
                exits,
                operator,
                MemoryAccess {
                    ptr,
                    offset,
                    width: I8X16,
                },
            );
            let value = b
                .ins()
                .load(I8X16, MemFlags::new().with_notrap(), address, 0);
            slots.set_vector(b, result, value);
        }
        VectorOp::Store {
            value,
            ptr,
            offset,
            lane,
        } => {
            let width = lane.map_or(I8X16, |(ty, _)| ty);
            let (address, relative) = memory.checked_address(
                b,
                slots,
                exits,
                operator,
                MemoryAccess { ptr, offset, width },
            );
            let stored = match lane {
                None => slots.vector(b, value, I8X16),
                Some((ty, lane)) => {
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
