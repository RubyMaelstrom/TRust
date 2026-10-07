#!/usr/bin/env python3
"""TRust: generates `src/engine/native_jit/decode_table.rs`.

The native region compiler decodes exactly the operators that the Wasmi executor
executes, with the semantics of the executor's handler. This script reads the
executor's handler tables (`src/engine/executor/handler/exec.rs` and
`exec/simd.rs`, which map every `OpCode` to its evaluation function) and the
generated operator decoding types of `wasmi_ir` (`decode.rs` in its build
output, built with the `simd` feature) and emits one match arm per operator
whose evaluation function has a native lowering.

Usage: gen_native_decode.py WASMI_DIR WASMI_IR_OUT_DIR > decode_table.rs
"""
import json, re, sys

wasmi, ir_out = sys.argv[1], sys.argv[2]
exec_dir = wasmi + '/src/engine/executor/handler/exec'

handlers = {}
for path in [exec_dir + '.rs', exec_dir + '/simd.rs']:
    source = open(path).read()
    for block in re.finditer(r'(?m)^([a-z_0-9]+)! \{\n(.*?)\n\}', source, re.S):
        family = block.group(1)
        for line in block.group(2).split('\n'):
            m = re.match(r'\s*fn ([a-z0-9_]+)\(([A-Za-z0-9_]+)(?:, ([^)]*))?\)(?: = (.*);)?', line)
            if m:
                handlers.setdefault(family, []).append((m.group(2), (m.group(3) or '').strip(), (m.group(4) or '').strip()))

decode_types = {}
for line in open(ir_out + '/decode.rs'):
    m = re.match(r'pub type (\w+) = (.*);', line)
    if m:
        decode_types[m.group(1)] = m.group(2)
    m = re.match(r'pub struct (\w+) \{', line)
    if m:
        decode_types.setdefault(m.group(1), 'struct')
op_codes = re.findall(r'^    (\w+),$', open(ir_out + '/op_code.rs').read().split('pub enum OpCode {')[1].split('}')[0], re.M)

arms = {}
simd_arms = {}

def arm(op, body, simd=False):
    (simd_arms if simd else arms)[op] = body

def d(op):
    return f'let d = dec::<ir::decode::{op}>(fields)?;'

INT = {'i32': 'types::I32', 'i64': 'types::I64'}
binary_kinds = {'add': 'Add', 'sub': 'Sub', 'mul': 'Mul', 'bitand': 'And', 'bitor': 'Or',
                'bitxor': 'Xor', 'shl': 'Shl', 'shr_s': 'ShrS', 'shr_u': 'ShrU', 'rotl': 'Rotl',
                'rotr': 'Rotr'}
compare_conds = {'eq': 'C::Int(IntCC::Equal)', 'ne': 'C::Int(IntCC::NotEqual)',
                 'lt_s': 'C::Int(IntCC::SignedLessThan)', 'lt_u': 'C::Int(IntCC::UnsignedLessThan)',
                 'le_s': 'C::Int(IntCC::SignedLessThanOrEqual)',
                 'le_u': 'C::Int(IntCC::UnsignedLessThanOrEqual)',
                 'and': 'C::And', 'not_and': 'C::NotAnd', 'or': 'C::Or', 'not_or': 'C::NotOr'}

def scalar_semantics(evaluate):
    """Returns ('binary', ty, kind) or ('compare', ty, cond) for an integer evaluation."""
    m = re.match(r'(?:wasm::|eval::wasmi_)(i32|i64|u32|u64)_(\w+?)(?:_ssi)?$', evaluate)
    if not m:
        return None
    width, name = m.group(1), m.group(2)
    ty = INT['i' + width[1:]]
    if name in binary_kinds:
        return ('binary', ty, 'B::' + binary_kinds[name])
    if width.startswith('u') and name == 'shr':
        return ('binary', ty, 'B::ShrU')
    if name == 'shr':
        return ('binary', ty, 'B::ShrS')
    if name in compare_conds:
        return ('compare', ty, compare_conds[name])
    return None

for op, _, evaluate in handlers['handler_unary']:
    copy32 = {'identity::<u32>', 'identity::<f32>', 'wasm::i32_wrap_i64', 'wasm::f32_reinterpret_i32',
              'wasm::i32_reinterpret_f32'}
    copy64 = {'identity::<u64>', 'identity::<f64>', 'wasm::f64_reinterpret_i64', 'wasm::i64_reinterpret_f64'}
    if evaluate in copy32 or evaluate in copy64:
        ty = 'types::I32' if evaluate in copy32 else 'types::I64'
        arm(op, f'{d(op)} N::Copy {{ ty: {ty}, result: d.result.dst(), value: d.value.src() }}')
        continue
    m = re.match(r'wasm::(i32|i64)_(clz|ctz|popcnt|extend8_s|extend16_s|extend32_s)$', evaluate)
    if m:
        kind = {'clz': 'U::Clz', 'ctz': 'U::Ctz', 'popcnt': 'U::Popcnt', 'extend8_s': 'U::Extend(types::I8)',
                'extend16_s': 'U::Extend(types::I16)', 'extend32_s': 'U::Extend(types::I32)'}[m.group(2)]
        arm(op, f'{d(op)} N::Unary {{ ty: {INT[m.group(1)]}, kind: {kind}, result: d.result.dst(), value: d.value.src() }}')
        continue
    # SIMD
    if evaluate == 'identity::<V128>':
        if decode_types[op].endswith('Slot>'):
            arm(op, f'{d(op)} V::Copy {{ result: d.result.cell(), input: d.value.cell() }}', True)
        else:
            arm(op, f'{d(op)} V::Constant {{ result: d.result.cell(), bytes: v128_bytes(d.value) }}', True)
        continue
    m = re.match(r'splat_(u8|u16|u32|u64|f32|f64)$', evaluate)
    if m:
        ty = {'u8': 'I8X16', 'u16': 'I16X8', 'u32': 'I32X4', 'f32': 'I32X4', 'u64': 'I64X2', 'f64': 'I64X2'}[m.group(1)]
        arm(op, f'{d(op)} V::Splat {{ result: d.result.cell(), ty: types::{ty}, value: d.value.src() }}', True)
        continue
    m = re.match(r'simd::v128_widen(8x8|16x4|32x2)_(s|u)$', evaluate)
    if m:
        ty = {'8x8': 'I16X8', '16x4': 'I32X4', '32x2': 'I64X2'}[m.group(1)]
        arm(op, f'{d(op)} V::Widen {{ result: d.result.cell(), ty: types::{ty}, signed: {str(m.group(2) == "s").lower()}, value: d.value.src() }}', True)
        continue
    m = re.match(r'simd::v128_low(32|64)_zero$', evaluate)
    if m:
        arm(op, f'{d(op)} V::LowZero {{ result: d.result.cell(), lane: types::I{m.group(1)}, value: d.value.src() }}', True)
        continue
    m = re.match(r'simd::(v128|i8x16|i16x8|i32x4|i64x2)_(any_true|all_true|bitmask)$', evaluate)
    if m:
        ty = {'v128': 'I8X16', 'i8x16': 'I8X16', 'i16x8': 'I16X8', 'i32x4': 'I32X4', 'i64x2': 'I64X2'}[m.group(1)]
        kind = {'any_true': 'Test::AnyTrue', 'all_true': 'Test::AllTrue', 'bitmask': 'Test::Bitmask'}[m.group(2)]
        arm(op, f'{d(op)} V::Test {{ result: d.result.dst(), kind: {kind}, ty: types::{ty}, input: d.value.cell() }}', True)
        continue
    unary = None
    m = re.match(r'simd::(\w+)$', evaluate)
    if m:
        name = m.group(1)
        table = {
            'v128_not': ('VU::Not', 'I8X16'),
            'i8x16_neg': ('VU::Neg', 'I8X16'), 'i16x8_neg': ('VU::Neg', 'I16X8'),
            'i32x4_neg': ('VU::Neg', 'I32X4'), 'i64x2_neg': ('VU::Neg', 'I64X2'),
            'i8x16_abs': ('VU::Abs', 'I8X16'), 'i16x8_abs': ('VU::Abs', 'I16X8'),
            'i32x4_abs': ('VU::Abs', 'I32X4'), 'i64x2_abs': ('VU::Abs', 'I64X2'),
            'i8x16_popcnt': ('VU::Popcnt', 'I8X16'),
            'f32x4_convert_i32x4_s': ('VU::ConvertI32 { signed: true }', 'I32X4'),
            'f32x4_convert_i32x4_u': ('VU::ConvertI32 { signed: false }', 'I32X4'),
            'f64x2_convert_low_i32x4_s': ('VU::ConvertLowI32 { signed: true }', 'I32X4'),
            'f64x2_convert_low_i32x4_u': ('VU::ConvertLowI32 { signed: false }', 'I32X4'),
            'i32x4_trunc_sat_f32x4_s': ('VU::TruncSatF32 { signed: true }', 'F32X4'),
            'i32x4_trunc_sat_f32x4_u': ('VU::TruncSatF32 { signed: false }', 'F32X4'),
            'i32x4_trunc_sat_f64x2_s_zero': ('VU::TruncSatF64Zero { signed: true }', 'F64X2'),
            'i32x4_trunc_sat_f64x2_u_zero': ('VU::TruncSatF64Zero { signed: false }', 'F64X2'),
            'f32x4_demote_f64x2_zero': ('VU::Demote', 'F64X2'),
            'f64x2_promote_low_f32x4': ('VU::Promote', 'F32X4'),
        }
        for lanes in ('f32x4', 'f64x2'):
            for fname, kind in [('abs', 'VU::FAbs'), ('neg', 'VU::FNeg'), ('sqrt', 'VU::Sqrt'), ('ceil', 'VU::Ceil'),
                                ('floor', 'VU::Floor'), ('trunc', 'VU::Trunc'), ('nearest', 'VU::Nearest')]:
                table[f'{lanes}_{fname}'] = (kind, lanes.upper())
        for wide, narrow in [('i16x8', 'I8X16'), ('i32x4', 'I16X8'), ('i64x2', 'I32X4')]:
            src = narrow.lower()
            for half in ('low', 'high'):
                for sign in ('s', 'u'):
                    table[f'{wide}_extend_{half}_{src}_{sign}'] = (
                        f'VU::Extend {{ high: {str(half == "high").lower()}, signed: {str(sign == "s").lower()} }}', narrow)
        for wide, narrow in [('i16x8', 'I8X16'), ('i32x4', 'I16X8')]:
            for sign in ('s', 'u'):
                table[f'{wide}_extadd_pairwise_{narrow.lower()}_{sign}'] = (
                    f'VU::ExtAddPairwise {{ signed: {str(sign == "s").lower()} }}', narrow)
        unary = table.get(name)
    if unary and decode_types[op] == 'UnaryOp<Slot, Slot>':
        arm(op, f'{d(op)} V::Unary {{ result: d.result.cell(), kind: {unary[0]}, ty: types::{unary[1]}, input: d.value.cell() }}', True)

for op, _, evaluate in handlers['handler_binary']:
    semantics = scalar_semantics(evaluate)
    if semantics and semantics[0] == 'binary':
        _, ty, kind = semantics
        arm(op, f'{d(op)} N::Binary {{ ty: {ty}, kind: {kind}, result: d.result.dst(), lhs: d.lhs.src(), rhs: d.rhs.src() }}')
        continue
    if semantics and semantics[0] == 'compare':
        _, ty, cond = semantics
        arm(op, f'{d(op)} N::Compare {{ result: d.result.dst(), comparison: Comparison {{ ty: {ty}, condition: {cond}, lhs: d.lhs.src(), rhs: d.rhs.src() }} }}')
        continue
    m = re.match(r'wrap_shift!\(simd::(i8x16|i16x8|i32x4|i64x2)_(shl|shr_s|shr_u)\)$', evaluate)
    if m:
        kind = {'shl': 'Shift::Shl', 'shr_s': 'Shift::ShrS', 'shr_u': 'Shift::ShrU'}[m.group(2)]
        arm(op, f'{d(op)} V::Shift {{ result: d.result.cell(), kind: {kind}, ty: types::{m.group(1).upper()}, lhs: d.lhs.cell(), amount: d.rhs.src() }}', True)
        continue
    m = re.match(r'simd::(\w+)$', evaluate)
    if not m or decode_types[op] != 'BinaryOp<Slot, Slot, Slot>':
        continue
    name = m.group(1)
    table = {
        'v128_and': ('VB::And', 'I8X16'), 'v128_or': ('VB::Or', 'I8X16'), 'v128_xor': ('VB::Xor', 'I8X16'),
        'v128_andnot': ('VB::AndNot', 'I8X16'), 'i8x16_swizzle': ('VB::Swizzle', 'I8X16'),
        'i16x8_q15mulr_sat_s': ('VB::Q15MulrSatS', 'I16X8'), 'i32x4_dot_i16x8_s': ('VB::Dot', 'I16X8'),
        'i8x16_narrow_i16x8_s': ('VB::Narrow { signed: true }', 'I16X8'),
        'i8x16_narrow_i16x8_u': ('VB::Narrow { signed: false }', 'I16X8'),
        'i16x8_narrow_i32x4_s': ('VB::Narrow { signed: true }', 'I32X4'),
        'i16x8_narrow_i32x4_u': ('VB::Narrow { signed: false }', 'I32X4'),
    }
    for lanes in ('i8x16', 'i16x8', 'i32x4', 'i64x2'):
        ty = lanes.upper()
        table[f'{lanes}_add'] = ('VB::Add', ty)
        table[f'{lanes}_sub'] = ('VB::Sub', ty)
        table[f'{lanes}_mul'] = ('VB::Mul', ty)
        for sign in ('s', 'u'):
            signed = str(sign == 's').lower()
            table[f'{lanes}_add_sat_{sign}'] = (f'VB::AddSat {{ signed: {signed} }}', ty)
            table[f'{lanes}_sub_sat_{sign}'] = (f'VB::SubSat {{ signed: {signed} }}', ty)
            table[f'{lanes}_min_{sign}'] = (f'VB::Min {{ signed: {signed} }}', ty)
            table[f'{lanes}_max_{sign}'] = (f'VB::Max {{ signed: {signed} }}', ty)
            cc = 'Signed' if sign == 's' else 'Unsigned'
            table[f'{lanes}_lt_{sign}'] = (f'VB::Icmp(IntCC::{cc}LessThan)', ty)
            table[f'{lanes}_le_{sign}'] = (f'VB::Icmp(IntCC::{cc}LessThanOrEqual)', ty)
        table[f'{lanes}_eq'] = ('VB::Icmp(IntCC::Equal)', ty)
        table[f'{lanes}_ne'] = ('VB::Icmp(IntCC::NotEqual)', ty)
        table[f'{lanes}_avgr_u'] = ('VB::AvgrU', ty)
    for wide, narrow in [('i16x8', 'I8X16'), ('i32x4', 'I16X8'), ('i64x2', 'I32X4')]:
        for half in ('low', 'high'):
            for sign in ('s', 'u'):
                table[f'{wide}_extmul_{half}_{narrow.lower()}_{sign}'] = (
                    f'VB::ExtMul {{ high: {str(half == "high").lower()}, signed: {str(sign == "s").lower()} }}', narrow)
    for lanes in ('f32x4', 'f64x2'):
        ty = lanes.upper()
        for fname, kind in [('add', 'VB::FAdd'), ('sub', 'VB::FSub'), ('mul', 'VB::FMul'), ('div', 'VB::FDiv'),
                            ('min', 'VB::FMin'), ('max', 'VB::FMax'), ('pmin', 'VB::FPmin'), ('pmax', 'VB::FPmax'),
                            ('eq', 'VB::Fcmp(FloatCC::Equal)'), ('ne', 'VB::Fcmp(FloatCC::NotEqual)'),
                            ('lt', 'VB::Fcmp(FloatCC::LessThan)'), ('le', 'VB::Fcmp(FloatCC::LessThanOrEqual)')]:
            table[f'{lanes}_{fname}'] = (kind, ty)
    binary = table.get(name)
    if binary:
        arm(op, f'{d(op)} V::Binary {{ result: d.result.cell(), kind: {binary[0]}, ty: types::{binary[1]}, lhs: d.lhs.cell(), rhs: d.rhs.cell() }}', True)

for op, _, evaluate in handlers['handler_cmp_branch']:
    semantics = scalar_semantics(evaluate)
    if semantics and semantics[0] == 'compare':
        _, ty, cond = semantics
        arm(op, f'{d(op)} N::Branch {{ comparison: Some(Comparison {{ ty: {ty}, condition: {cond}, lhs: d.lhs.src(), rhs: d.rhs.src() }}), target: target(address, d.offset) }}')

for op, _, hint in handlers['handler_select']:
    ty = {'u32': 'types::I32', 'f32': 'types::I32', 'u64': 'types::I64', 'f64': 'types::I64'}[hint]
    arm(op, f'{d(op)} N::Select {{ ty: {ty}, result: d.result.dst(), condition: d.condition.src(), values: [d.true_val.src(), d.false_val.src()] }}')
for op, _, _ in handlers['execution_handler_for_v128_select']:
    arm(op, f'{d(op)} V::Select {{ result: d.result.cell(), condition: d.condition.src(), values: [d.true_val.cell(), d.false_val.cell()] }}', True)

loads = {
    'load_u32': ('I32', 'I32', 'false'), 'load_u64': ('I64', 'I64', 'false'),
    'load_f32': ('I32', 'I32', 'false'), 'load_f64': ('I64', 'I64', 'false'),
    'i32_load8_s': ('I8', 'I32', 'true'), 'i32_load8_u': ('I8', 'I32', 'false'),
    'i32_load16_s': ('I16', 'I32', 'true'), 'i32_load16_u': ('I16', 'I32', 'false'),
    'i64_load8_s': ('I8', 'I64', 'true'), 'i64_load8_u': ('I8', 'I64', 'false'),
    'i64_load16_s': ('I16', 'I64', 'true'), 'i64_load16_u': ('I16', 'I64', 'false'),
    'i64_load32_s': ('I32', 'I64', 'true'), 'i64_load32_u': ('I32', 'I64', 'false'),
}
for family, form in [('handler_load', 'op'), ('handler_load_ri', 'at'), ('handler_load_mem0_offset16', 'mem0')]:
    for op, _, evaluate in handlers[family]:
        name = evaluate.split('::')[-1].removesuffix('_at')
        if name == 'v128_load':
            access = {'op': 'mem(d.memory)?; let offset = static_offset(d.offset)?; let ptr = d.ptr.src();',
                      'mem0': 'let offset = u64::from(d.offset) as u32; let ptr = d.ptr.src();'}[form]
            arm(op, f'{d(op)} {access} V::Load {{ result: d.result.cell(), ptr, offset }}', True)
            continue
        if name not in loads:
            continue
        width, ty, signed = loads[name]
        access = {'op': 'mem(d.memory)?; let offset = static_offset(d.offset)?; let ptr = d.ptr.src();',
                  'at': 'mem(d.memory)?; let ptr = constant_address(d.address); let offset = 0;',
                  'mem0': 'let offset = u64::from(d.offset) as u32; let ptr = d.ptr.src();'}[form]
        arm(op, f'{d(op)} {access} N::Load {{ result: d.result.dst(), ptr, offset, width: types::{width}, ty: types::{ty}, signed: {signed} }}')

store_widths = {'u32': 'I32', 'u64': 'I64', 'f32': 'I32', 'f64': 'I64', 'i8': 'I8', 'i16': 'I16', 'i32': 'I32'}
for family, form in [('handler_store', 'op'), ('handler_store_ix', 'at'), ('handler_store_mem0_offset16', 'mem0')]:
    for op, hint, _ in handlers[family]:
        access = {'op': 'mem(d.memory)?; let offset = static_offset(d.offset)?; let ptr = d.ptr.src();',
                  'at': 'mem(d.memory)?; let ptr = constant_address(d.address); let offset = 0;',
                  'mem0': 'let offset = u64::from(d.offset) as u32; let ptr = d.ptr.src();'}[form]
        if hint == 'V128':
            arm(op, f'{d(op)} {access} V::Store {{ value: d.value.cell(), ptr, offset, lane: None }}', True)
            continue
        arm(op, f'{d(op)} {access} N::Store {{ value: d.value.src(), ptr, offset, width: types::{store_widths[hint]} }}')

for family, form in [('handler_store_lane_ss', 'op'), ('handler_store_lane_mem0_offset16_ss', 'mem0')]:
    for op, width, evaluate in handlers[family]:
        lane_ty = {'1': 'I8', '2': 'I16'}[width]
        access = {'op': 'mem(d.memory)?; let offset = u32::try_from(d.offset).ok()?; let ptr = d.ptr.src();',
                  'mem0': 'let offset = u64::from(d.offset) as u32; let ptr = d.ptr.src();'}[form]
        arm(op, f'{d(op)} {access} V::Store {{ value: d.value.cell(), ptr, offset, lane: Some((types::{lane_ty}, u8::from(d.lane))) }}', True)

for op, ty, _ in handlers['global_get_execution_handler']:
    if ty == 'V128':
        arm(op, f'{d(op)} V::GlobalGet {{ result: d.result.cell(), global: d.global }}', True)
    else:
        arm(op, f'{d(op)} N::GlobalGet {{ result: d.result.dst(), global: d.global }}')
for op, ty, _ in handlers['global_set_execution_handler']:
    if ty == 'V128':
        arm(op, f'{d(op)} V::GlobalSet {{ value: d.value.cell(), global: d.global }}', True)
    else:
        width = 'types::I32' if ty in ('u32', 'f32') else 'types::I64'
        arm(op, f'{d(op)} N::GlobalSet {{ ty: {width}, value: d.value.src(), global: d.global }}')

for op, _, evaluate in handlers['handler_extract_lane']:
    m = re.match(r'simd::(i8x16|i16x8|i32x4|i64x2|f32x4|f64x2)_extract_lane(_s|_u)?$', evaluate)
    if m:
        ty = {'i8x16': 'I8X16', 'i16x8': 'I16X8', 'i32x4': 'I32X4', 'i64x2': 'I64X2', 'f32x4': 'I32X4', 'f64x2': 'I64X2'}[m.group(1)]
        signed = str(m.group(2) == '_s').lower()
        arm(op, f'{d(op)} V::Extract {{ result: d.result.dst(), ty: types::{ty}, lane: u8::from(d.lane), input: d.value.cell(), signed: {signed} }}', True)
        continue
    m = re.match(r'(?:v128_replace_lane(8x16|16x8|32x4|64x2)|(f32x4|f64x2)_replace_lane)$', evaluate)
    if m:
        ty = {'8x16': 'I8X16', '16x8': 'I16X8', '32x4': 'I32X4', '64x2': 'I64X2', 'f32x4': 'I32X4', 'f64x2': 'I64X2'}[m.group(1) or m.group(2)]
        arm(op, f'{d(op)} V::Replace {{ result: d.result.cell(), ty: types::{ty}, lane: u8::from(d.lane), input: d.v128.cell(), value: d.value.src() }}', True)

for op, _, evaluate in handlers['handler_ternary']:
    if evaluate == 'simd::v128_bitselect':
        arm(op, f'{d(op)} V::Bitselect {{ result: d.result.cell(), lhs: d.a.cell(), rhs: d.b.cell(), mask: d.c.cell() }}', True)
    elif evaluate == 'simd::i8x16_shuffle':
        arm(op, f'{d(op)} V::Shuffle {{ result: d.result.cell(), lhs: d.lhs.cell(), rhs: d.rhs.cell(), lanes: d.selector.map(u8::from) }}', True)

arm('Branch', 'let d = dec::<ir::decode::Branch>(fields)?; N::Branch { comparison: None, target: target(address, d.offset) }')

out = []
out.append('// Generated by `trust/gen_native_decode.py` from the executor handler tables and')
out.append('// the `wasmi_ir` operator decoding types. Do not edit by hand.')
out.append('')
out.append('/// Decodes the fields of the operator `code` at `address` into its native operation.')
out.append('///')
out.append('/// Returns `None` for operators without a native lowering.')
out.append('#[allow(clippy::too_many_lines)]')
out.append('pub(super) fn decode_native(code: OpCode, fields: &mut &[u8], address: usize) -> Option<NativeOp> {')
out.append('    Some(match code {')
for op in op_codes:
    if op in arms:
        out.append(f'        OpCode::{op} => {{ {arms[op]} }}')
out.append('        #[cfg(feature = "simd")]')
out.append('        _ => return decode_vector(code, fields, address).map(NativeOp::Vector),')
out.append('        #[cfg(not(feature = "simd"))]')
out.append('        _ => return None,')
out.append('    })')
out.append('}')
out.append('')
out.append('/// Decodes the fields of the v128 operator `code` into its native operation.')
out.append('#[cfg(feature = "simd")]')
out.append('#[allow(clippy::too_many_lines)]')
out.append('fn decode_vector(code: OpCode, fields: &mut &[u8], _address: usize) -> Option<VectorOp> {')
out.append('    Some(match code {')
for op in op_codes:
    if op in simd_arms:
        out.append(f'        OpCode::{op} => {{ {simd_arms[op]} }}')
out.append('        _ => return None,')
out.append('    })')
out.append('}')
out.append('')
out.append('/// Decodes and skips the fields of any operator `code` except branch tables, whose')
out.append('/// targets follow the operator.')
out.append('#[cfg(all(test, feature = "simd"))]')
out.append('#[allow(clippy::too_many_lines)]')
out.append('pub(super) fn skip_fields(code: OpCode, fields: &mut &[u8]) -> Option<()> {')
out.append('    match code {')
for op in op_codes:
    if op.startswith('BranchTable'):
        out.append(f'        OpCode::{op} => return None,')
    elif op in decode_types:
        out.append(f'        OpCode::{op} => {{ dec::<ir::decode::{op}>(fields)?; }}')
out.append('        _ => {}')
out.append('    }')
out.append('    Some(())')
out.append('}')
print('\n'.join(out))
print(f'// scalar: {len(arms)}, vector: {len(simd_arms)}, operators: {len(op_codes)}', file=sys.stderr)
