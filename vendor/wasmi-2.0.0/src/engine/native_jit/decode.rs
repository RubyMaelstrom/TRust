//! Decoding of encoded Wasmi operators into native operations.
//!
//! An encoded operator is its handler address (or op-code with the
//! `indirect-dispatch` feature) followed by its fields, padded so that the next
//! operator is aligned relative to it, exactly as the executor decodes it.
//! Every supported operator keeps the semantics of its executor handler; the
//! generated table maps handlers' evaluation functions to native operations.

#[cfg(feature = "simd")]
use super::simd::{Binary as VB, Shift, Test, Unary as VU, VectorOp, VectorOp as V};
use super::{
    Binary as B,
    Comparison,
    Condition as C,
    Dest,
    NativeOp,
    NativeOp as N,
    Operand,
    Reg,
    Unary as U,
};
use crate::{
    core::ShiftAmount,
    engine::executor::op_code_to_handler,
    ir::{self, BranchOffset, Local, OpCode, Slot, SlotAndReg},
};
#[cfg(feature = "simd")]
use cranelift_codegen::ir::condcodes::FloatCC;
use cranelift_codegen::ir::{condcodes::IntCC, types};
use core::num::NonZero;
use std::{collections::HashMap, sync::OnceLock};

include!("decode_table.rs");

/// Decodes the fields of an operator of type `T`.
fn dec<T: ir::Decode>(fields: &mut &[u8]) -> Option<T> {
    T::decode(fields).ok()
}

/// The frame cell index of `slot`.
fn cell(slot: Slot) -> u32 {
    (slot.byte_offset() / 8) as u32
}

/// An operator field usable as a scalar input.
trait Src {
    fn src(self) -> Operand;
}

/// An operator field usable as a scalar destination.
trait Dst {
    fn dst(self) -> Dest;
}

/// An operator field addressing the first cell of a v128.
#[cfg(feature = "simd")]
trait Cell {
    fn cell(self) -> u32;
}

impl Src for Slot {
    fn src(self) -> Operand {
        Operand::Slot(cell(self))
    }
}

impl Dst for Slot {
    fn dst(self) -> Dest {
        Dest::Slot(cell(self))
    }
}

#[cfg(feature = "simd")]
impl Cell for Slot {
    fn cell(self) -> u32 {
        cell(self)
    }
}

impl<const INDEX: u16> Src for Local<INDEX> {
    fn src(self) -> Operand {
        Operand::Slot(u32::from(INDEX))
    }
}

impl<const INDEX: u16> Dst for Local<INDEX> {
    fn dst(self) -> Dest {
        Dest::Slot(u32::from(INDEX))
    }
}

macro_rules! impl_reg {
    ( $( $ty:ty => $reg:expr ),* $(,)? ) => {
        $(
            impl Src for ir::Reg<$ty> {
                fn src(self) -> Operand {
                    Operand::Reg($reg)
                }
            }

            impl Dst for ir::Reg<$ty> {
                fn dst(self) -> Dest {
                    Dest::Reg($reg)
                }
            }

            impl Dst for SlotAndReg<$ty> {
                fn dst(self) -> Dest {
                    Dest::Both(cell(self.slot), $reg)
                }
            }
        )*
    };
}
impl_reg!(i64 => Reg::I, f32 => Reg::F32, f64 => Reg::F64);

macro_rules! impl_constant {
    ( $( $ty:ty ),* $(,)? ) => {
        $(
            impl Src for $ty {
                #[allow(clippy::cast_lossless)]
                fn src(self) -> Operand {
                    Operand::Constant(self as i64)
                }
            }

            impl Src for NonZero<$ty> {
                fn src(self) -> Operand {
                    self.get().src()
                }
            }
        )*
    };
}
impl_constant!(i8, i16, i32, i64, u8, u16, u32, u64);

impl Src for f32 {
    fn src(self) -> Operand {
        Operand::Constant(i64::from(self.to_bits()))
    }
}

impl Src for f64 {
    fn src(self) -> Operand {
        Operand::Constant(self.to_bits() as i64)
    }
}

impl Src for ShiftAmount {
    fn src(self) -> Operand {
        Operand::Constant(i64::from(u8::from(self)))
    }
}

/// The absolute address of the branch target `offset` from the operator at `address`.
fn target(address: usize, offset: BranchOffset) -> usize {
    address.wrapping_add_signed(i32::from(offset) as isize)
}

/// Accepts only the default linear memory, whose bytes the region receives.
fn mem(memory: ir::MemoryAddr) -> Option<()> {
    (u32::from(memory) == 0).then_some(())
}

/// Accepts static offsets that fit the 32-bit offsets of native accesses.
fn static_offset(offset: ir::Offset) -> Option<u32> {
    u32::try_from(u64::from(offset)).ok()
}

/// A constant effective address as a pointer operand with zero offset.
fn constant_address(address: ir::Address) -> Operand {
    Operand::Constant(u64::from(address) as i64)
}

#[cfg(feature = "simd")]
fn v128_bytes(value: crate::V128) -> [u8; 16] {
    value.as_u128().to_le_bytes()
}

/// The op-codes of all operator handlers, by handler address.
fn handlers() -> &'static HashMap<usize, OpCode> {
    static HANDLERS: OnceLock<HashMap<usize, OpCode>> = OnceLock::new();
    HANDLERS.get_or_init(|| {
        (0..ir::LEN_OPS)
            .filter_map(|code| OpCode::new(code as u16))
            .map(|code| (op_code_to_handler(code) as usize, code))
            .collect()
    })
}

/// The op-code of the operator at the start of `bytes` and the size of its header.
fn op_code(bytes: &[u8]) -> Option<(OpCode, usize)> {
    const HEADER: usize = core::mem::size_of::<usize>();
    if cfg!(feature = "indirect-dispatch") {
        let header = bytes.get(..2)?;
        let code = OpCode::new(u16::from_ne_bytes([header[0], header[1]]))?;
        return Some((code, 2));
    }
    let header = bytes.get(..HEADER)?;
    let handler = usize::from_ne_bytes(header.try_into().ok()?);
    Some((*handlers().get(&handler)?, HEADER))
}

/// The encoded length of an operator whose header and fields span `len` bytes.
fn aligned(len: usize) -> usize {
    match cfg!(feature = "indirect-dispatch") {
        true => len,
        false => len.next_multiple_of(core::mem::align_of::<usize>()),
    }
}

/// Decodes the operator at the start of `bytes`, located at `address`.
///
/// Returns the native operation and the address of the next operator, or the
/// op-code of an operator without a native lowering (`None` if unknown).
pub(super) fn decode_at(bytes: &[u8], address: usize) -> Result<(NativeOp, usize), Option<OpCode>> {
    let Some((code, header)) = op_code(bytes) else {
        return Err(None);
    };
    let mut fields = &bytes[header..];
    let op = decode_native(code, &mut fields, address).ok_or(Some(code))?;
    let used = bytes.len() - fields.len();
    Ok((op, address + aligned(used)))
}

/// The encoded length of the operator at the start of `bytes`, if known.
#[cfg(all(test, feature = "simd"))]
pub(super) fn op_len_at(bytes: &[u8]) -> Option<usize> {
    let (code, header) = op_code(bytes)?;
    let mut fields = &bytes[header..];
    skip_fields(code, &mut fields)?;
    Some(aligned(bytes.len() - fields.len()))
}
