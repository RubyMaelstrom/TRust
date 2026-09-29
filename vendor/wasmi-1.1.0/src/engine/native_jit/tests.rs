use crate::{Config, Engine, Linker, Module as WasmModule, Store};
use alloc::{format, string::String};

fn run(source: &str, enabled: bool, input: (i32, i32)) -> (i32, Engine) {
    let mut config = Config::default();
    config.native_jit(enabled);
    let engine = Engine::new(&config);
    let bytes = wat::parse_str(source).unwrap();
    let module = WasmModule::new(&engine, bytes).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    let function = instance
        .get_typed_func::<(i32, i32), i32>(&store, "run")
        .unwrap();
    (function.call(&mut store, input).unwrap(), engine)
}

fn compiled_regions(engine: &Engine) -> usize {
    engine
        .inner
        .code_map
        .native_jit
        .regions
        .lock()
        .unwrap()
        .values()
        .filter(|candidate| candidate.region.is_some())
        .count()
}

#[test]
fn native_short_host_calls_accumulate_hotness_across_stores() {
    // The exported-call boundary in the Wasm JS API does not reset a function's
    // semantics or its engine-owned code. No individual call visits a PC twice.
    let source = r#"(module
        (memory (export "memory") 1)
        (global $g (mut i32) (i32.const 0))
        (func (export "run") (param $v i32) (result i32)
            global.get $g local.get $v i32.add global.set $g
            i32.const 0 global.get $g i32.store
            i32.const 0 i32.load))"#;
    for enabled in [false, true] {
        let mut config = Config::default();
        config.native_jit(enabled);
        let engine = Engine::new(&config);
        let module = WasmModule::new(&engine, wat::parse_str(source).unwrap()).unwrap();
        let mut stores = [Store::new(&engine, ()), Store::new(&engine, ())];
        let instances = stores.each_mut().map(|store| {
            Linker::new(&engine)
                .instantiate_and_start(store, &module)
                .unwrap()
        });
        let functions = std::array::from_fn::<_, 2, _>(|i| {
            instances[i]
                .get_typed_func::<i32, i32>(&stores[i], "run")
                .unwrap()
        });
        let mut sums = [0_i32; 2];
        for call in 0..100 {
            let i = call % 2;
            let value = (call as i32).wrapping_mul(7919);
            sums[i] = sums[i].wrapping_add(value);
            assert_eq!(functions[i].call(&mut stores[i], value).unwrap(), sums[i]);
            let memory = instances[i].get_memory(&stores[i], "memory").unwrap();
            assert_eq!(&memory.data(&stores[i])[..4], &sums[i].to_le_bytes());
            assert!(!memory.take_dirty_ranges(&mut stores[i]).is_empty());
            if call < 32 {
                assert_eq!(compiled_regions(&engine), 0);
            }
        }
        assert_eq!(compiled_regions(&engine) > 0, enabled);
    }
}

#[test]
fn native_global_loop_and_budget_exit_match_interpreter() {
    let source = r#"(module
        (global $g (mut i32) (i32.const 0))
        (func (export "run") (param $n i32) (param $v i32) (result i32)
            (loop $again
                global.get $g local.get $v i32.xor local.get $n i32.add global.set $g
                local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
            global.get $g))"#;
    for value in [0, 1, -1, i32::MIN, i32::MAX] {
        let expected = run(source, false, (50_000, value)).0;
        let (actual, engine) = run(source, true, (50_000, value));
        assert_eq!(actual, expected);
        assert!(
            compiled_regions(&engine) > 0,
            "the native path must have compiled"
        );
    }
}

#[test]
fn native_cold_startup_cannot_exhaust_future_hotness() {
    let mut source = String::from("(module");
    for index in 0..super::MAX_CANDIDATES + 32 {
        source.push_str(&format!(
            "(func (export \"cold{index}\") (param i32) (result i32) local.get 0 i32.const {index} i32.add)"
        ));
    }
    source.push_str(
        "(memory 1) (global $g (mut i32) (i32.const 7919))
         (func (export \"hot\") (param i32) (result i32)
             i32.const 0 local.get 0 i32.store
             i32.const 0 i32.load global.get $g i32.mul))",
    );
    let mut config = Config::default();
    config.native_jit(true);
    let engine = Engine::new(&config);
    let module = WasmModule::new(&engine, wat::parse_str(source).unwrap()).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    for index in 0..super::MAX_CANDIDATES + 32 {
        let function = instance
            .get_typed_func::<i32, i32>(&store, &format!("cold{index}"))
            .unwrap();
        assert_eq!(function.call(&mut store, 17).unwrap(), index as i32 + 17);
    }
    assert_eq!(compiled_regions(&engine), 0);
    let hot = instance.get_typed_func::<i32, i32>(&store, "hot").unwrap();
    for value in 0..100 {
        assert_eq!(hot.call(&mut store, value).unwrap(), value * 7919);
    }
    assert!(compiled_regions(&engine) > 0);
    assert!(
        engine
            .inner
            .code_map
            .native_jit
            .regions
            .lock()
            .unwrap()
            .len()
            <= super::MAX_CANDIDATES
    );
}

#[test]
fn native_integer_widths_shifts_and_signed_branches_match_interpreter() {
    // Core numerics: shifts/rotations use the count modulo the operand width;
    // signed comparison and wrapping arithmetic apply before each branch.
    let source = r#"(module
        (func (export "run") (param $n i32) (param $v i32) (result i32)
            (loop $again
                local.get $v local.get $n i32.rotl
                local.get $v i32.const 33 i32.shr_s i32.xor
                i32.const 2147483647 i32.mul local.set $v
                local.get $n i32.const 1 i32.sub local.tee $n i32.const 0 i32.gt_s br_if $again)
            local.get $v))"#;
    for value in [0, -1, i32::MIN, i32::MAX, 0x12345678] {
        let expected = run(source, false, (50_000, value)).0;
        let (actual, engine) = run(source, true, (50_000, value));
        assert_eq!(actual, expected);
        assert!(compiled_regions(&engine) > 0);
    }
}

#[test]
fn native_fuel_metering_keeps_interpreter_accounting() {
    let mut config = Config::default();
    config.native_jit(true).consume_fuel(true);
    let engine = Engine::new(&config);
    assert!(!engine.inner.code_map.native_jit.enabled);
    let bytes = wat::parse_str("(module (func (export \"run\") (loop $again br $again)))").unwrap();
    let module = WasmModule::new(&engine, bytes).unwrap();
    let mut store = Store::new(&engine, ());
    store.set_fuel(1_000).unwrap();
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    let function = instance.get_typed_func::<(), ()>(&store, "run").unwrap();
    assert!(function.call(&mut store, ()).unwrap_err().is_out_of_fuel());
    assert_eq!(compiled_regions(&engine), 0);
}

#[test]
fn native_memory_boundaries_and_dirty_writes_before_a_trap() {
    for enabled in [false, true] {
        let mut config = Config::default();
        config.native_jit(enabled);
        let engine = Engine::new(&config);
        let bytes = wat::parse_str(
            r#"(module
            (memory (export "memory") 1)
            (func (export "run") (param $n i32) (param $ptr i32) (result i32)
                (local $sum i32)
                (loop $again
                    local.get $ptr local.get $n i32.store8
                    local.get $ptr i32.load16_u local.get $sum i32.xor local.set $sum
                    local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
                local.get $sum))"#,
        )
        .unwrap();
        let module = WasmModule::new(&engine, bytes).unwrap();
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .unwrap();
        let function = instance
            .get_typed_func::<(i32, i32), i32>(&store, "run")
            .unwrap();
        let memory = instance.get_memory(&store, "memory").unwrap();
        assert_eq!(function.call(&mut store, (50_000, 65_534)).unwrap(), 80);
        memory.take_dirty_ranges(&mut store);
        let before = memory.data_version(&store);
        let error = function.call(&mut store, (99, 65_535)).unwrap_err();
        assert_eq!(
            error.as_trap_code(),
            Some(crate::TrapCode::MemoryOutOfBounds)
        );
        assert_eq!(
            memory.data(&store)[65_535],
            99,
            "the valid packed store precedes the invalid load"
        );
        assert_ne!(memory.data_version(&store), before);
        let dirty_ranges = memory.take_dirty_ranges(&mut store);
        assert_eq!(dirty_ranges.len(), 1);
        assert_eq!(dirty_ranges[0], 0..65_536);
        let before = memory.data_version(&store);
        for ptr in [65_536, -1, i32::MIN] {
            assert!(function.call(&mut store, (99, ptr)).is_err());
        }
        assert_eq!(
            memory.data_version(&store),
            before,
            "trapping stores cannot dirty memory"
        );
        if enabled {
            assert!(compiled_regions(&engine) > 0);
        }
    }
}

#[test]
fn native_memory64_effective_address_must_not_wrap() {
    let engine = Engine::default();
    let bytes = wat::parse_str(
        r#"(module
        (memory i64 1)
        (func (export "run") (param $n i32) (param $ptr i64) (result i32)
            (local $sum i32)
            (loop $again
                local.get $ptr local.get $n i32.store8 offset=17
                local.get $ptr i32.load8_u offset=17 local.get $sum i32.xor local.set $sum
                local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
            local.get $sum))"#,
    )
    .unwrap();
    let module = WasmModule::new(&engine, bytes).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    let function = instance
        .get_typed_func::<(i32, i64), i32>(&store, "run")
        .unwrap();
    assert_eq!(function.call(&mut store, (50_000, 65_518)).unwrap(), 80);
    assert!(compiled_regions(&engine) > 0);
    for ptr in [65_519, -1, -17, i64::MIN] {
        assert_eq!(
            function
                .call(&mut store, (99, ptr))
                .unwrap_err()
                .as_trap_code(),
            Some(crate::TrapCode::MemoryOutOfBounds)
        );
    }
}

#[test]
fn native_memory_signed_loads_and_unaligned_packed_stores_match() {
    for (store, load, result) in [
        ("i32.store8", "i32.load8_s", -1),
        ("i32.store16", "i32.load16_s", -1),
        ("i32.store16", "i32.load16_u", 65_535),
        ("i32.store", "i32.load", -1),
    ] {
        let source = std::format!(
            r#"(module
            (memory 1)
            (func (export "run") (param $n i32) (param $v i32) (result i32)
                (loop $again
                    i32.const 7 local.get $v {store}
                    i32.const 7 {load} local.set $v
                    local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
                local.get $v))"#
        );
        assert_eq!(run(&source, false, (10_000, -1)).0, result);
        let (actual, engine) = run(&source, true, (10_000, -1));
        assert_eq!(actual, result);
        assert!(compiled_regions(&engine) > 0);
    }
}

#[test]
fn native_select_full_values_and_conversions_match_interpreter() {
    let source = r#"(module
        (func (export "run") (param $n i32) (param $v i32) (result i32)
            (local $wide i64)
            local.get $v i64.extend_i32_s local.set $wide
            (loop $again
                local.get $wide i64.const 1234567890123456789
                local.get $n i32.const 7 i32.eq select
                i64.const 47 i64.rotl i64.const -9223372036854775808 i64.xor local.set $wide
                local.get $wide i32.wrap_i64 i32.extend8_s local.set $v
                local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
            local.get $v))"#;
    for value in [0, -1, i32::MIN, i32::MAX] {
        let expected = run(source, false, (50_000, value)).0;
        let (actual, engine) = run(source, true, (50_000, value));
        assert_eq!(actual, expected);
        assert!(compiled_regions(&engine) > 0);
    }
}

#[test]
fn native_aliased_globals_and_host_relocation_preserve_order() {
    use crate::{Caller, Global, Mutability, Val};
    let source = r#"(module
        (import "env" "g" (global $g (mut i32)))
        (import "env" "alias" (global $alias (mut i32)))
        (import "env" "hook" (func $hook))
        (func (export "run") (param $n i32) (result i32)
            (loop $again
                global.get $g i32.const 1 i32.add global.set $alias
                global.get $g i32.const 3 i32.mul global.set $g
                local.get $n i32.const 100 i32.eq
                if call $hook end
                local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
            global.get $alias))"#;
    let mut results = std::vec::Vec::new();
    for enabled in [false, true] {
        let mut config = Config::default();
        config.native_jit(enabled);
        let engine = Engine::new(&config);
        let bytes = wat::parse_str(source).unwrap();
        let module = WasmModule::new(&engine, bytes).unwrap();
        let mut store = Store::new(&engine, None::<Global>);
        let global = Global::new(&mut store, Val::I32(0), Mutability::Var);
        *store.data_mut() = Some(global);
        let mut linker = Linker::new(&engine);
        linker.define("env", "g", global).unwrap();
        linker.define("env", "alias", global).unwrap();
        linker
            .func_wrap("env", "hook", |mut caller: Caller<Option<Global>>| {
                // Force the global arena to move while preserving the imported
                // handles, then mutate through the shared alias.
                for _ in 0..8192 {
                    Global::new(&mut caller, Val::I32(0), Mutability::Var);
                }
                caller
                    .data()
                    .unwrap()
                    .set(&mut caller, Val::I32(123))
                    .unwrap();
            })
            .unwrap();
        let instance = linker.instantiate_and_start(&mut store, &module).unwrap();
        let function = instance.get_typed_func::<i32, i32>(&store, "run").unwrap();
        results.push(function.call(&mut store, 50_000).unwrap());
        if enabled {
            assert!(compiled_regions(&engine) > 0);
        }
    }
    assert_eq!(results[0], results[1]);
}

#[cfg(feature = "simd")]
#[test]
fn native_select_and_globals_preserve_both_halves_of_v128() {
    // Core #exec-select selects the complete value, including both 64-bit
    // halves of a v128. Extraction itself remains an interpreter boundary.
    let source = r#"(module
        (global $a (mut v128) (v128.const i64x2 1234567890123456789 -2345678901234567890))
        (global $b (mut v128) (v128.const i64x2 -3456789012345678901 4567890123456789012))
        (global $result (mut v128) (v128.const i64x2 0 0))
        (func (export "run") (param $n i32) (param $choice i32) (result i64 i64)
            (loop $again
                global.get $a global.get $b
                local.get $n local.get $choice i32.eq select
                global.set $result
                local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
            global.get $result i64x2.extract_lane 0
            global.get $result i64x2.extract_lane 1))"#;
    for enabled in [false, true] {
        let mut config = Config::default();
        config.native_jit(enabled);
        let engine = Engine::new(&config);
        let module = WasmModule::new(&engine, wat::parse_str(source).unwrap()).unwrap();
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .unwrap();
        let function = instance
            .get_typed_func::<(i32, i32), (i64, i64)>(&store, "run")
            .unwrap();
        for (choice, expected) in [
            (0, (-3456789012345678901, 4567890123456789012)),
            (1, (1234567890123456789, -2345678901234567890)),
        ] {
            assert_eq!(
                function.call(&mut store, (50_000, choice)).unwrap(),
                expected
            );
        }
        if enabled {
            assert!(compiled_regions(&engine) > 0);
        }
    }
}

#[test]
fn native_memory_growth_and_nondefault_memory_use_fresh_views() {
    let source = r#"(module
        (memory (export "first") 1 2)
        (memory (export "second") 1)
        (func (export "run") (param $n i32) (param $v i32) (result i32)
            (loop $again
                i32.const 0 local.get $v i32.store
                i32.const 0 i32.load local.set $v
                i32.const 4 local.get $v i32.store
                i32.const 4 i32.load local.set $v
                i32.const 0 local.get $v i32.store 1
                i32.const 0 i32.load 1 i32.const 1 i32.add local.set $v
                local.get $n i32.const 100 i32.eq
                if i32.const 1 memory.grow drop end
                local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
            local.get $v))"#;
    let expected = run(source, false, (50_000, i32::MAX)).0;
    let (actual, engine) = run(source, true, (50_000, i32::MAX));
    assert_eq!(actual, expected);
    assert!(compiled_regions(&engine) > 0);
}

#[test]
fn native_cache_limit_falls_back_without_changing_results() {
    use super::MAX_REGIONS;
    let mut source = std::string::String::from("(module (global $g (mut i32) (i32.const 0))");
    for i in 0..MAX_REGIONS + 4 {
        source.push_str(&std::format!(
            r#"
            (func (export "run{i}") (param $n i32) (result i32)
                i32.const 0 global.set $g
                (loop $again
                    global.get $g local.get $n i32.xor global.set $g
                    local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
                global.get $g)"#
        ));
    }
    source.push(')');
    let engine = Engine::default();
    let bytes = wat::parse_str(source).unwrap();
    let module = WasmModule::new(&engine, bytes).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    for i in 0..MAX_REGIONS + 4 {
        let function = instance
            .get_typed_func::<i32, i32>(&store, &std::format!("run{i}"))
            .unwrap();
        assert_eq!(function.call(&mut store, 1_000).unwrap(), 1_000);
    }
    assert_eq!(compiled_regions(&engine), MAX_REGIONS);
}

#[test]
fn native_hot_leaf_calls_compile_without_an_inner_loop() {
    let source = r#"(module
        (func $leaf (param $x i32) (result i32)
            local.get $x i32.const 12345 i32.xor
            i32.const 13 i32.rotl i32.const 6789 i32.mul
            i32.const 7 i32.shr_u i32.const -1 i32.xor)
        (func (export "run") (param $n i32) (param $v i32) (result i32)
            (loop $again
                local.get $v call $leaf local.set $v
                local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
            local.get $v))"#;
    let expected = run(source, false, (50_000, 123)).0;
    let (actual, engine) = run(source, true, (50_000, 123));
    assert_eq!(actual, expected);
    assert!(compiled_regions(&engine) > 0);
}

#[test]
fn native_forward_branches_merge_slot_values() {
    let source = r#"(module
        (func (export "run") (param $n i32) (param $v i32) (result i32)
            (loop $again
                local.get $n i32.const 1000 i32.lt_u
                if
                    local.get $v i32.const 17 i32.add local.set $v
                else
                    local.get $v i32.const 31 i32.xor local.set $v
                end
                local.get $n i32.const 73 i32.eq
                if
                    local.get $v i32.const -1 i32.mul local.set $v
                end
                local.get $n i32.const 1 i32.sub local.tee $n br_if $again)
            local.get $v))"#;
    for value in [0, -1, i32::MIN, i32::MAX] {
        let expected = run(source, false, (5000, value)).0;
        let (actual, engine) = run(source, true, (5000, value));
        assert_eq!(actual, expected);
        assert!(compiled_regions(&engine) > 0);
    }
}

#[test]
fn native_many_alternating_functions_keep_their_hotness() {
    use crate::engine::EngineFunc;
    // More simultaneously hot functions than the original direct-mapped
    // table could hold. Every function must eventually reach the compiler,
    // independently of allocator alignment and the order of calls.
    let mut source = std::string::String::from("(module");
    for i in 0..100 {
        source.push_str(&std::format!(
            r#"
            (func $f{i} (param $x i32) (result i32)
                local.get $x i32.const {i} i32.xor
                i32.const 13 i32.rotl i32.const 6789 i32.mul
                i32.const 7 i32.shr_u i32.const -1 i32.xor)"#
        ));
    }
    source.push_str(
        r#"(func (export "run") (param $n i32) (param $v i32) (result i32) (loop $again "#,
    );
    for i in 0..100 {
        source.push_str(&std::format!("local.get $v call $f{i} local.set $v\n"));
    }
    source.push_str("local.get $n i32.const 1 i32.sub local.tee $n br_if $again) local.get $v))");
    let expected = run(&source, false, (500, 123)).0;
    let (actual, engine) = run(&source, true, (500, 123));
    assert_eq!(actual, expected);
    let regions = engine.inner.code_map.native_jit.regions.lock().unwrap();
    for index in 0..100 {
        let function = engine
            .inner
            .code_map
            .get(None, EngineFunc::from_u32(index))
            .unwrap();
        assert!(
            regions
                .values()
                .filter_map(|candidate| candidate.region.as_ref())
                .any(|region| region.function_base == function.instrs().as_ptr() as usize),
            "function {index} never reached native compilation"
        );
    }
}

#[test]
#[ignore = "local Wasm compilation diagnostic; requires WASMI_JIT_MODULE"]
fn native_compile_module_diagnostic() {
    let path = std::env::var("WASMI_JIT_MODULE").expect("WASMI_JIT_MODULE path");
    let bytes = std::fs::read(path).unwrap();
    let count = wasmparser::Parser::new(0)
        .parse_all(&bytes)
        .find_map(|payload| match payload.unwrap() {
            wasmparser::Payload::CodeSectionStart { count, .. } => Some(count),
            _ => None,
        })
        .unwrap();
    let mut config = Config::default();
    config.compilation_mode(crate::CompilationMode::Eager);
    let engine = Engine::new(&config);
    let _module = WasmModule::new(&engine, bytes).unwrap();
    let mut eligible = 0;
    let mut compiled = 0;
    for index in 0..count {
        let function = engine
            .inner
            .code_map
            .get(None, crate::engine::EngineFunc::from_u32(index))
            .unwrap();
        let instrs = function.instrs();
        if instrs.iter().take(4).all(|op| super::decode(*op).is_some()) {
            eligible += 1;
            if super::compile(instrs, 0).is_some() {
                compiled += 1;
            }
        }
    }
    std::eprintln!("functions={count}, eligible_prefixes={eligible}, compiled_prefixes={compiled}");
}
