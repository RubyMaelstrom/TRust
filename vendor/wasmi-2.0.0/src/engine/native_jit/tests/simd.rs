//! Native-vs-interpreter differential tests for v128 regions.
//!
//! Each instruction under test runs in a hot loop whose operands come from a
//! vector generator mixing pseudo-random lanes with boundary values (lane
//! minima and maxima, zero, NaN payloads, signed zeros and infinities). The
//! loop's accumulated result must match the interpreter exactly, and the loop
//! itself must have been compiled natively. Float results are canonicalized
//! first: Core #aux-nans permits any NaN payload for computed NaNs.

use super::{compiled_regions, function_ops};
use crate::{Config, Engine, Linker, Module as WasmModule, Store};
use alloc::{format, string::String, vec::Vec};

/// Lane-boundary inputs; `bitselect` mixes them into the generated lanes.
const SPECIAL_INT: &str = "(v128.const i8x16 0x80 0x7f 0xff 0x00 0x01 0x80 0xff 0x7f \
     0x00 0x80 0x00 0x00 0xff 0xff 0xff 0x7f)";
const SPECIAL_F32: &str = "(v128.const i32x4 0x7fc00001 0x80000000 0x7f800000 0xff800000)";
const SPECIAL_F64: &str = "(v128.const i64x2 0xfff4000000000001 0x8000000000000000)";
const SPECIAL_F32_LARGE: &str = "(v128.const f32x4 2147483648 -2147483904 4294967296 -0.5)";
const SPECIAL_F64_LARGE: &str = "(v128.const f64x2 4294967295.5 -2147483648.75)";

/// Float shape of a result that must be NaN-canonicalized before comparison.
#[derive(Clone, Copy)]
enum Float {
    No,
    F32,
    F64,
}

/// One function per instruction: `body` computes a v128 from `$x`, `$y`,
/// `$z` (v128) and `$k` (i32), and is folded into the accumulator.
fn module(cases: &[(&str, Float)]) -> String {
    let mut source = String::from("(module (memory 1)");
    for (index, (body, float)) in cases.iter().enumerate() {
        let canonical = match float {
            Float::No => String::from("(local.get $c)"),
            Float::F32 => String::from(
                "(v128.bitselect (v128.const i32x4 0x7fc00000 0x7fc00000 0x7fc00000 0x7fc00000) \
                 (local.get $c) (f32x4.ne (local.get $c) (local.get $c)))",
            ),
            Float::F64 => String::from(
                "(v128.bitselect (v128.const i64x2 0x7ff8000000000000 0x7ff8000000000000) \
                 (local.get $c) (f64x2.ne (local.get $c) (local.get $c)))",
            ),
        };
        source.push_str(&format!(
            r#"
            (func (export "f{index}") (param $n i32) (param $seed i64) (result i64 i64)
                (local $x v128) (local $y v128) (local $z v128) (local $c v128)
                (local $s v128) (local $acc v128) (local $k i32)
                (local.set $s (i64x2.splat (local.get $seed)))
                (loop $again
                    ;; Advance a 32-bit LCG per lane and derive operands from it.
                    (local.set $s (i32x4.add
                        (i32x4.mul (local.get $s) (v128.const i32x4 1664525 22695477 1103515245 134775813))
                        (v128.const i32x4 1013904223 1 12345 2531011)))
                    (local.set $x (v128.bitselect {SPECIAL_INT} (local.get $s)
                        (i8x16.lt_s (local.get $s) (v128.const i8x16 -96 -96 -96 -96 -96 -96 -96 -96 -96 -96 -96 -96 -96 -96 -96 -96))))
                    (local.set $y (i8x16.shuffle 7 1 12 3 15 5 0 10 8 13 2 11 4 9 14 6 (local.get $s) (local.get $x)))
                    (local.set $y (v128.bitselect {SPECIAL_F32} (local.get $y)
                        (i32x4.gt_u (local.get $s) (v128.const i32x4 0xd0000000 0xd0000000 0xd0000000 0xd0000000))))
                    (local.set $z (v128.bitselect {SPECIAL_F64} (i64x2.shl (local.get $s) (i32.const 13))
                        (i64x2.lt_s (local.get $x) (v128.const i64x2 0 0))))
                    (local.set $z (v128.bitselect {SPECIAL_F32_LARGE} (local.get $z)
                        (i32x4.lt_u (local.get $s) (v128.const i32x4 0x20000000 0x20000000 0x20000000 0x20000000))))
                    (local.set $x (v128.bitselect {SPECIAL_F64_LARGE} (local.get $x)
                        (i64x2.gt_s (local.get $y) (v128.const i64x2 0x6000000000000000 0x6000000000000000))))
                    (local.set $k (i32x4.extract_lane 3 (local.get $s)))
                    (local.set $c {body})
                    (local.set $c {canonical})
                    (local.set $acc (i64x2.add
                        (v128.xor (local.get $acc) (local.get $c))
                        (i8x16.shuffle 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 0 (local.get $acc) (local.get $acc))))
                    (br_if $again (local.tee $n (i32.sub (local.get $n) (i32.const 1)))))
                (i64x2.extract_lane 0 (local.get $acc))
                (i64x2.extract_lane 1 (local.get $acc)))"#,
        ));
    }
    source.push(')');
    source
}

/// Runs every case with and without native code; returns the native engine.
fn run_cases(cases: &[(&str, Float)]) -> Vec<(i64, i64)> {
    let source = module(cases);
    let bytes = wat::parse_str(&source).unwrap_or_else(|error| panic!("{error}\n{source}"));
    let mut results = Vec::new();
    for enabled in [false, true] {
        let mut config = Config::default();
        config.native_jit(enabled);
        let engine = Engine::new(&config);
        let module = WasmModule::new(&engine, &bytes[..]).unwrap();
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .unwrap();
        let mut outputs = Vec::new();
        for index in 0..cases.len() {
            let function = instance
                .get_typed_func::<(i32, i64), (i64, i64)>(&store, &format!("f{index}"))
                .unwrap();
            for seed in [0x0123_4567_89ab_cdef_i64, -1] {
                outputs.push(function.call(&mut store, (1_000, seed)).unwrap());
            }
        }
        if enabled {
            assert_loops_native(&engine, cases.len() as u32);
        } else {
            assert_eq!(compiled_regions(&engine), 0);
        }
        results.push(outputs);
    }
    for (index, (interpreted, native)) in results[0].iter().zip(&results[1]).enumerate() {
        assert_eq!(
            interpreted,
            native,
            "native result differs for `{}`",
            cases[index / 2].0
        );
    }
    results.pop().unwrap()
}

/// Every loop of the first `count` functions lies inside one native region.
fn assert_loops_native(engine: &Engine, count: u32) {
    let regions = engine.inner.native_jit.regions.lock().unwrap();
    let compiled: Vec<_> = regions
        .values()
        .filter_map(|candidate| candidate.region.as_ref())
        .collect();
    for index in 0..count {
        let ops = function_ops(engine, index);
        let base = ops.as_ptr() as usize;
        let mut address = base;
        while address < base + ops.len() {
            let bytes = &ops[address - base..];
            let Ok((op, next)) = super::super::decode::decode_at(bytes, address) else {
                match super::super::decode::op_len_at(bytes) {
                    Some(len) => {
                        address += len;
                        continue;
                    }
                    None => break,
                }
            };
            if let super::super::NativeOp::Branch { target, .. } = op {
                if target <= address {
                    assert!(
                        compiled.iter().any(|region| {
                            region.function_start == base
                                && region.start <= target
                                && region.end > address
                        }),
                        "loop +{}..=+{} of function {index} is not native",
                        target - base,
                        address - base,
                    );
                }
            }
            address = next;
        }
    }
}

fn binary(ops: &[&str], float: Float) -> Vec<(String, Float)> {
    ops.iter()
        .flat_map(|op| {
            [
                (format!("({op} (local.get $x) (local.get $y))"), float),
                (format!("({op} (local.get $z) (local.get $x))"), float),
            ]
        })
        .collect()
}

fn unary(ops: &[&str], float: Float) -> Vec<(String, Float)> {
    ops.iter()
        .flat_map(|op| {
            [
                (format!("({op} (local.get $x))"), float),
                (format!("({op} (local.get $z))"), float),
            ]
        })
        .collect()
}

fn check(cases: Vec<(String, Float)>) {
    // At most 64 functions per engine stay well below the region cache cap.
    for chunk in cases.chunks(64) {
        let cases: Vec<(&str, Float)> = chunk
            .iter()
            .map(|(body, float)| (body.as_str(), *float))
            .collect();
        run_cases(&cases);
        lower_on_all_targets(&cases);
    }
}

/// The interpreter comparison runs on this host only; also compile every
/// region for both native-JIT targets: baseline x86-64 (SSE2), x86-64-v2
/// (SSE4.2), x86-64-v3 (AVX2) and AArch64 (NEON).
fn lower_on_all_targets(cases: &[(&str, Float)]) {
    let mut config = Config::default();
    config.compilation_mode(crate::CompilationMode::Eager);
    let engine = Engine::new(&config);
    let bytes = wat::parse_str(module(cases)).unwrap();
    let _module = WasmModule::new(&engine, &bytes[..]).unwrap();
    for index in 0..cases.len() as u32 {
        let ops = function_ops(&engine, index);
        for (isa, preset) in [
            ("x86_64", "x86-64"),
            ("x86_64", "x86-64-v2"),
            ("x86_64", "x86-64-v3"),
            ("aarch64", ""),
        ] {
            let compiled = super::super::lower_for(isa, preset, ops).unwrap_or_else(|error| {
                panic!("`{}` on {preset}: {error}", cases[index as usize].0)
            });
            assert!(compiled > 0);
        }
    }
}

#[test]
fn native_simd_integer_lanewise_arithmetic_matches_interpreter() {
    let mut cases = binary(
        &[
            "i8x16.add",
            "i8x16.sub",
            "i8x16.add_sat_s",
            "i8x16.add_sat_u",
            "i8x16.sub_sat_s",
            "i8x16.sub_sat_u",
            "i8x16.min_s",
            "i8x16.min_u",
            "i8x16.max_s",
            "i8x16.max_u",
            "i8x16.avgr_u",
            "i16x8.add",
            "i16x8.sub",
            "i16x8.mul",
            "i16x8.add_sat_s",
            "i16x8.add_sat_u",
            "i16x8.sub_sat_s",
            "i16x8.sub_sat_u",
            "i16x8.min_s",
            "i16x8.min_u",
            "i16x8.max_s",
            "i16x8.max_u",
            "i16x8.avgr_u",
            "i16x8.q15mulr_sat_s",
            "i32x4.add",
            "i32x4.sub",
            "i32x4.mul",
            "i32x4.min_s",
            "i32x4.min_u",
            "i32x4.max_s",
            "i32x4.max_u",
            "i32x4.dot_i16x8_s",
            "i64x2.add",
            "i64x2.sub",
            "i64x2.mul",
        ],
        Float::No,
    );
    cases.extend(unary(
        &[
            "i8x16.abs",
            "i8x16.neg",
            "i8x16.popcnt",
            "i16x8.abs",
            "i16x8.neg",
            "i32x4.abs",
            "i32x4.neg",
            "i64x2.abs",
            "i64x2.neg",
        ],
        Float::No,
    ));
    check(cases);
}

#[test]
fn native_simd_widening_narrowing_and_pairwise_ops_match_interpreter() {
    let mut cases = binary(
        &[
            "i8x16.narrow_i16x8_s",
            "i8x16.narrow_i16x8_u",
            "i16x8.narrow_i32x4_s",
            "i16x8.narrow_i32x4_u",
            "i16x8.extmul_low_i8x16_s",
            "i16x8.extmul_high_i8x16_s",
            "i16x8.extmul_low_i8x16_u",
            "i16x8.extmul_high_i8x16_u",
            "i32x4.extmul_low_i16x8_s",
            "i32x4.extmul_high_i16x8_s",
            "i32x4.extmul_low_i16x8_u",
            "i32x4.extmul_high_i16x8_u",
            "i64x2.extmul_low_i32x4_s",
            "i64x2.extmul_high_i32x4_s",
            "i64x2.extmul_low_i32x4_u",
            "i64x2.extmul_high_i32x4_u",
        ],
        Float::No,
    );
    cases.extend(unary(
        &[
            "i16x8.extend_low_i8x16_s",
            "i16x8.extend_high_i8x16_s",
            "i16x8.extend_low_i8x16_u",
            "i16x8.extend_high_i8x16_u",
            "i32x4.extend_low_i16x8_s",
            "i32x4.extend_high_i16x8_s",
            "i32x4.extend_low_i16x8_u",
            "i32x4.extend_high_i16x8_u",
            "i64x2.extend_low_i32x4_s",
            "i64x2.extend_high_i32x4_s",
            "i64x2.extend_low_i32x4_u",
            "i64x2.extend_high_i32x4_u",
            "i16x8.extadd_pairwise_i8x16_s",
            "i16x8.extadd_pairwise_i8x16_u",
            "i32x4.extadd_pairwise_i16x8_s",
            "i32x4.extadd_pairwise_i16x8_u",
        ],
        Float::No,
    ));
    check(cases);
}

#[test]
fn native_simd_bitwise_comparisons_and_reductions_match_interpreter() {
    let mut cases = binary(
        &[
            "v128.and",
            "v128.or",
            "v128.xor",
            "v128.andnot",
            "i8x16.eq",
            "i8x16.ne",
            "i8x16.lt_s",
            "i8x16.lt_u",
            "i8x16.gt_s",
            "i8x16.gt_u",
            "i8x16.le_s",
            "i8x16.le_u",
            "i8x16.ge_s",
            "i8x16.ge_u",
            "i16x8.eq",
            "i16x8.ne",
            "i16x8.lt_s",
            "i16x8.lt_u",
            "i16x8.gt_s",
            "i16x8.gt_u",
            "i16x8.le_s",
            "i16x8.le_u",
            "i16x8.ge_s",
            "i16x8.ge_u",
            "i32x4.eq",
            "i32x4.ne",
            "i32x4.lt_s",
            "i32x4.lt_u",
            "i32x4.gt_s",
            "i32x4.gt_u",
            "i32x4.le_s",
            "i32x4.le_u",
            "i32x4.ge_s",
            "i32x4.ge_u",
            "i64x2.eq",
            "i64x2.ne",
            "i64x2.lt_s",
            "i64x2.gt_s",
            "i64x2.le_s",
            "i64x2.ge_s",
            "f32x4.eq",
            "f32x4.ne",
            "f32x4.lt",
            "f32x4.gt",
            "f32x4.le",
            "f32x4.ge",
            "f64x2.eq",
            "f64x2.ne",
            "f64x2.lt",
            "f64x2.gt",
            "f64x2.le",
            "f64x2.ge",
        ],
        Float::No,
    );
    cases.extend(unary(&["v128.not"], Float::No));
    for op in [
        "v128.any_true",
        "i8x16.all_true",
        "i16x8.all_true",
        "i32x4.all_true",
        "i64x2.all_true",
        "i8x16.bitmask",
        "i16x8.bitmask",
        "i32x4.bitmask",
        "i64x2.bitmask",
    ] {
        // Reductions produce an i32; fold it into one lane of the result.
        for operand in ["$x", "$y", "(i8x16.eq (local.get $x) (local.get $x))"] {
            let operand = if operand.starts_with('$') {
                format!("(local.get {operand})")
            } else {
                String::from(operand)
            };
            cases.push((
                format!("(i32x4.replace_lane 1 (local.get $y) ({op} {operand}))"),
                Float::No,
            ));
        }
    }
    cases.push((
        String::from("(v128.bitselect (local.get $x) (local.get $y) (local.get $z))"),
        Float::No,
    ));
    check(cases);
}

#[test]
fn native_simd_shifts_mask_their_count_by_lane_width() {
    // Core #op-ishl/#op-ishr: the count is taken modulo the lane width, both
    // for a dynamic count and for counts the translator folds into the op.
    let mut cases = Vec::new();
    for op in [
        "i8x16.shl",
        "i8x16.shr_s",
        "i8x16.shr_u",
        "i16x8.shl",
        "i16x8.shr_s",
        "i16x8.shr_u",
        "i32x4.shl",
        "i32x4.shr_s",
        "i32x4.shr_u",
        "i64x2.shl",
        "i64x2.shr_s",
        "i64x2.shr_u",
    ] {
        cases.push((format!("({op} (local.get $x) (local.get $k))"), Float::No));
        cases.push((
            format!("({op} (local.get $y) (i32.and (local.get $k) (i32.const 127)))"),
            Float::No,
        ));
        // Wasmi folds a constant count modulo the lane width at translation.
        for count in [1, 9, 33, -1] {
            cases.push((
                format!("({op} (local.get $x) (i32.const {count}))"),
                Float::No,
            ));
        }
    }
    check(cases);
}

#[test]
fn native_simd_float_arithmetic_and_conversions_match_interpreter() {
    let mut cases = binary(
        &[
            "f32x4.add",
            "f32x4.sub",
            "f32x4.mul",
            "f32x4.div",
            "f32x4.min",
            "f32x4.max",
            "f32x4.pmin",
            "f32x4.pmax",
        ],
        Float::F32,
    );
    cases.extend(binary(
        &[
            "f64x2.add",
            "f64x2.sub",
            "f64x2.mul",
            "f64x2.div",
            "f64x2.min",
            "f64x2.max",
            "f64x2.pmin",
            "f64x2.pmax",
        ],
        Float::F64,
    ));
    cases.extend(unary(
        &[
            "f32x4.abs",
            "f32x4.neg",
            "f32x4.sqrt",
            "f32x4.ceil",
            "f32x4.floor",
            "f32x4.trunc",
            "f32x4.nearest",
            "f32x4.convert_i32x4_s",
            "f32x4.convert_i32x4_u",
            "f32x4.demote_f64x2_zero",
        ],
        Float::F32,
    ));
    cases.extend(unary(
        &[
            "f64x2.abs",
            "f64x2.neg",
            "f64x2.sqrt",
            "f64x2.ceil",
            "f64x2.floor",
            "f64x2.trunc",
            "f64x2.nearest",
            "f64x2.convert_low_i32x4_s",
            "f64x2.convert_low_i32x4_u",
            "f64x2.promote_low_f32x4",
        ],
        Float::F64,
    ));
    cases.extend(unary(
        &[
            "i32x4.trunc_sat_f32x4_s",
            "i32x4.trunc_sat_f32x4_u",
            "i32x4.trunc_sat_f64x2_s_zero",
            "i32x4.trunc_sat_f64x2_u_zero",
        ],
        Float::No,
    ));
    check(cases);
}

#[test]
fn native_simd_shuffle_swizzle_splat_and_lanes_match_interpreter() {
    let mut cases = Vec::new();
    for mask in [
        "0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15",
        "31 30 29 28 27 26 25 24 23 22 21 20 19 18 17 16",
        "1 2 3 0 5 6 7 4 9 10 11 8 13 14 15 12",
        "0 16 1 17 2 18 3 19 4 20 5 21 6 22 7 23",
        "4 5 6 7 4 5 6 7 4 5 6 7 4 5 6 7",
        "0 1 2 3 4 5 6 7 24 25 26 27 28 29 30 31",
        "3 31 0 17 9 9 22 5 14 28 11 1 19 30 7 16",
    ] {
        cases.push((
            format!("(i8x16.shuffle {mask} (local.get $x) (local.get $y))"),
            Float::No,
        ));
        cases.push((
            format!("(i8x16.shuffle {mask} (local.get $z) (local.get $z))"),
            Float::No,
        ));
    }
    // Core #op-iswizzle-lane: selector lanes >= 16 produce zero.
    cases.push((
        String::from("(i8x16.swizzle (local.get $x) (local.get $y))"),
        Float::No,
    ));
    cases.push((
        String::from(
            "(i8x16.swizzle (local.get $x) (v128.and (local.get $y) \
             (v128.const i8x16 15 31 15 31 15 31 15 31 15 31 15 31 15 31 15 31)))",
        ),
        Float::No,
    ));
    for (shape, lanes, scalar) in [
        ("i8x16", 16, "(local.get $k)"),
        ("i16x8", 8, "(local.get $k)"),
        ("i32x4", 4, "(local.get $k)"),
        ("i64x2", 2, "(i64.extend_i32_s (local.get $k))"),
        ("f32x4", 4, "(f32.reinterpret_i32 (local.get $k))"),
        (
            "f64x2",
            2,
            "(f64.reinterpret_i64 (i64.extend_i32_s (local.get $k)))",
        ),
    ] {
        cases.push((format!("({shape}.splat {scalar})"), Float::No));
        for lane in [0, lanes / 2 - 1, lanes - 1] {
            cases.push((
                format!("({shape}.replace_lane {lane} (local.get $x) {scalar})"),
                Float::No,
            ));
        }
    }
    for (op, lane, fold) in [
        (
            "i8x16.extract_lane_s",
            15,
            "(i32x4.replace_lane 0 (local.get $y) X)",
        ),
        (
            "i8x16.extract_lane_u",
            7,
            "(i32x4.replace_lane 0 (local.get $y) X)",
        ),
        (
            "i16x8.extract_lane_s",
            7,
            "(i32x4.replace_lane 2 (local.get $y) X)",
        ),
        (
            "i16x8.extract_lane_u",
            3,
            "(i32x4.replace_lane 2 (local.get $y) X)",
        ),
        (
            "i32x4.extract_lane",
            3,
            "(i32x4.replace_lane 1 (local.get $y) X)",
        ),
        (
            "i64x2.extract_lane",
            1,
            "(i64x2.replace_lane 0 (local.get $y) X)",
        ),
        (
            "f32x4.extract_lane",
            2,
            "(f32x4.replace_lane 3 (local.get $y) X)",
        ),
        (
            "f64x2.extract_lane",
            1,
            "(f64x2.replace_lane 1 (local.get $y) X)",
        ),
    ] {
        cases.push((
            fold.replace('X', &format!("({op} {lane} (local.get $x))")),
            Float::No,
        ));
    }
    // Immediate replacement values use the operand-word encodings.
    for body in [
        "(i8x16.replace_lane 9 (local.get $x) (i32.const -3))",
        "(i16x8.replace_lane 5 (local.get $x) (i32.const 40000))",
        "(i32x4.replace_lane 2 (local.get $x) (i32.const -123456789))",
        "(i64x2.replace_lane 1 (local.get $x) (i64.const -987654321))",
        "(f32x4.replace_lane 1 (local.get $x) (f32.const -1.5))",
        "(f64x2.replace_lane 0 (local.get $x) (f64.const 0.25))",
    ] {
        cases.push((String::from(body), Float::No));
    }
    check(cases);
}

#[test]
fn native_simd_memory_accesses_match_interpreter_and_trap_in_place() {
    // Unaligned v128 accesses (the memarg alignment is only a hint), every
    // load shape, and lane stores. The final iteration reaches the end of
    // memory: the out-of-bounds access must leave the region before it
    // touches memory, and earlier stores in that iteration stay visible.
    let source = r#"(module
        (memory (export "memory") 1)
        (func (export "run") (param $n i32) (param $ptr i32) (result i64 i64)
            (local $v v128) (local $acc v128)
            (local.set $v (v128.const i32x4 0x01020304 0x8899aabb 0xfedcba98 0x7f80ff00))
            (loop $again
                (v128.store offset=3 align=1 (local.get $ptr) (local.get $v))
                (local.set $acc (v128.xor (local.get $acc) (v128.load offset=1 (local.get $ptr))))
                (local.set $acc (i16x8.add (local.get $acc) (v128.load8x8_s offset=2 (local.get $ptr))))
                (local.set $acc (i32x4.add (local.get $acc) (v128.load16x4_u offset=4 (local.get $ptr))))
                (local.set $acc (i64x2.add (local.get $acc) (v128.load32x2_s offset=5 (local.get $ptr))))
                (local.set $acc (v128.xor (local.get $acc) (v128.load8_splat offset=6 (local.get $ptr))))
                (local.set $acc (v128.xor (local.get $acc) (v128.load16_splat offset=7 (local.get $ptr))))
                (local.set $acc (v128.xor (local.get $acc) (v128.load32_zero offset=8 (local.get $ptr))))
                (local.set $acc (v128.xor (local.get $acc) (v128.load64_zero offset=9 (local.get $ptr))))
                (local.set $acc (v128.load32_lane offset=10 1 (local.get $ptr) (local.get $acc)))
                (local.set $acc (v128.load8_lane 13 (local.get $ptr) (local.get $acc)))
                (v128.store16_lane offset=1 3 (local.get $ptr) (local.get $acc))
                (v128.store64_lane 1 (local.get $ptr) (local.get $acc))
                (local.set $v (i8x16.shuffle 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 0
                    (i32x4.add (local.get $v) (local.get $acc)) (local.get $v)))
                (local.set $ptr (i32.add (local.get $ptr) (i32.const 1)))
                (br_if $again (local.tee $n (i32.sub (local.get $n) (i32.const 1)))))
            (i64x2.extract_lane 0 (local.get $acc))
            (i64x2.extract_lane 1 (local.get $acc))))"#;
    let mut results = Vec::new();
    for enabled in [false, true] {
        let mut config = Config::default();
        config.native_jit(enabled);
        let engine = Engine::new(&config);
        let module = WasmModule::new(&engine, wat::parse_str(source).unwrap()).unwrap();
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .unwrap();
        let run = instance
            .get_typed_func::<(i32, i32), (i64, i64)>(&store, "run")
            .unwrap();
        let memory = instance.get_memory(&store, "memory").unwrap();
        let mut outcome = Vec::new();
        outcome.push(format!("{:?}", run.call(&mut store, (20_000, 7))));
        // The v128.store at ptr+3 fits until ptr = 65517; the 16-byte load at
        // ptr+1 still fits, but the earlier store faults at ptr = 65518.
        let error = run.call(&mut store, (100, 65_500)).unwrap_err();
        outcome.push(format!("{:?}", error.as_trap_code()));
        outcome.push(format!("{:?}", &memory.data(&store)[65_400..]));
        if enabled {
            assert_loops_native(&engine, 1);
        }
        results.push(outcome);
    }
    assert_eq!(
        results[1][1],
        format!("{:?}", Some(crate::TrapCode::MemoryOutOfBounds))
    );
    assert_eq!(results[0], results[1]);
}

#[test]
fn native_simd_slots_mix_scalar_and_vector_values() {
    // The same temporaries hold i32 and v128 values within one region; scalar
    // writes keep the slot's high word (Wasmi `WriteAs`), selects and globals
    // move complete values, and the loop outlives the backedge budget so the
    // vector state is flushed and reloaded across region re-entries.
    let source = r#"(module
        (global $g (mut v128) (v128.const i64x2 0x0123456789abcdef -2))
        (func (export "run") (param $n i32) (param $choice i32) (result i64 i64)
            (local $v v128) (local $w v128) (local $i i32)
            (local.set $v (global.get $g))
            (loop $again
                (local.set $i (i32.add (i32x4.extract_lane 2 (local.get $v)) (local.get $n)))
                (local.set $w (i32x4.replace_lane 0 (local.get $v) (local.get $i)))
                (local.set $v (select (result v128) (local.get $w)
                    (i64x2.add (local.get $v) (global.get $g))
                    (i32.eq (i32.and (local.get $n) (i32.const 3)) (local.get $choice))))
                (global.set $g (v128.xor (global.get $g) (i32x4.shl (local.get $v) (local.get $i))))
                (local.set $v (i8x16.shuffle 3 0 1 2 7 4 5 6 11 8 9 10 15 12 13 14 (local.get $v) (local.get $w)))
                (br_if $again (local.tee $n (i32.sub (local.get $n) (i32.const 1)))))
            (i64x2.extract_lane 0 (local.get $v))
            (i64x2.extract_lane 1 (global.get $g))))"#;
    for choice in [0, 1, 3] {
        let mut results = Vec::new();
        for enabled in [false, true] {
            let mut config = Config::default();
            config.native_jit(enabled);
            let engine = Engine::new(&config);
            let module = WasmModule::new(&engine, wat::parse_str(source).unwrap()).unwrap();
            let mut store = Store::new(&engine, ());
            let instance = Linker::new(&engine)
                .instantiate_and_start(&mut store, &module)
                .unwrap();
            let run = instance
                .get_typed_func::<(i32, i32), (i64, i64)>(&store, "run")
                .unwrap();
            results.push(run.call(&mut store, (50_000, choice)).unwrap());
            if enabled {
                assert_loops_native(&engine, 1);
            }
        }
        assert_eq!(results[0], results[1]);
    }
}
