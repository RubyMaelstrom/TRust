//! Runs the official WebAssembly SIMD scripts (`test/core/simd/simd_*.wast`
//! of the spec repository) with the interpreter and with every control-flow
//! entry compiled natively at its first visit (`NativeJit::eager`).
//!
//! WASMI_SPEC_DIR=/path/to/spec/test/core/simd cargo test ... native_simd_spec -- --ignored

use crate::{
    Config,
    Engine,
    Extern,
    Global,
    Instance,
    Linker,
    Memory,
    MemoryType,
    Module,
    Mutability,
    Nullable,
    Ref,
    RefType,
    Store,
    Table,
    TableType,
    V128,
    Val,
};
use alloc::{format, string::String, vec::Vec};
use std::{collections::HashMap, path::Path};
use wast::{
    QuoteWat, Wast, WastArg, WastDirective, WastExecute, WastInvoke, WastRet,
    core::{NanPattern, V128Pattern, WastArgCore, WastRetCore},
    parser::{self, ParseBuffer},
};

struct Runner {
    store: Store<()>,
    linker: Linker<()>,
    last: Option<Instance>,
    named: HashMap<String, Instance>,
}

impl Runner {
    fn new(native: bool) -> Self {
        let mut config = Config::default();
        config.native_jit(native);
        let engine = Engine::new(&config);
        if native {
            engine.inner.native_jit.set_eager(true);
        }
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        // Core test harness `spectest` module.
        let globals = [
            ("global_i32", Val::I32(666)),
            ("global_i64", Val::I64(666)),
            ("global_f32", Val::F32(666.6_f32.into())),
            ("global_f64", Val::F64(666.6_f64.into())),
        ];
        for (name, value) in globals {
            let global = Global::new(&mut store, value, Mutability::Const);
            linker.define("spectest", name, global).unwrap();
        }
        let memory = Memory::new(&mut store, MemoryType::new(1, Some(2))).unwrap();
        linker.define("spectest", "memory", memory).unwrap();
        let table = Table::new(
            &mut store,
            TableType::new(RefType::Func, 10, Some(20)),
            Ref::Func(Nullable::Null),
        )
        .unwrap();
        linker.define("spectest", "table", table).unwrap();
        linker.func_wrap("spectest", "print", || {}).unwrap();
        linker
            .func_wrap("spectest", "print_i32", |_: i32| {})
            .unwrap();
        linker
            .func_wrap("spectest", "print_i64", |_: i64| {})
            .unwrap();
        linker
            .func_wrap("spectest", "print_f32", |_: f32| {})
            .unwrap();
        linker
            .func_wrap("spectest", "print_f64", |_: f64| {})
            .unwrap();
        linker
            .func_wrap("spectest", "print_i32_f32", |_: i32, _: f32| {})
            .unwrap();
        linker
            .func_wrap("spectest", "print_f64_f64", |_: f64, _: f64| {})
            .unwrap();
        Self {
            store,
            linker,
            last: None,
            named: HashMap::new(),
        }
    }

    fn instantiate(&mut self, bytes: &[u8]) -> Result<Instance, String> {
        let module = Module::new(self.store.engine(), bytes).map_err(|e| format!("{e}"))?;
        self.linker
            .instantiate_and_start(&mut self.store, &module)
            .map_err(|e| format!("{e}"))
    }

    fn instance(&self, id: Option<wast::token::Id<'_>>) -> Result<Instance, String> {
        match id {
            Some(id) => self
                .named
                .get(id.name())
                .copied()
                .ok_or("unknown module".into()),
            None => self.last.ok_or("no module".into()),
        }
    }

    fn invoke(&mut self, invoke: &WastInvoke<'_>) -> Result<Vec<Val>, String> {
        let instance = self.instance(invoke.module)?;
        let func = instance
            .get_func(&self.store, invoke.name)
            .ok_or_else(|| format!("unknown export {}", invoke.name))?;
        let args = invoke
            .args
            .iter()
            .map(argument)
            .collect::<Result<Vec<_>, _>>()?;
        let ty = func.ty(&self.store);
        let mut results: Vec<Val> = ty.results().iter().copied().map(Val::default_for_ty).collect();
        func.call(&mut self.store, &args, &mut results)
            .map_err(|e| format!("trap: {e}"))?;
        Ok(results)
    }

    fn execute(&mut self, exec: &mut WastExecute<'_>) -> Result<Vec<Val>, String> {
        match exec {
            WastExecute::Invoke(invoke) => self.invoke(invoke),
            WastExecute::Get { module, global, .. } => {
                let instance = self.instance(*module)?;
                match instance.get_export(&self.store, global) {
                    Some(Extern::Global(global)) => Ok(Vec::from([global.get(&self.store)])),
                    _ => Err(format!("unknown global {global}")),
                }
            }
            WastExecute::Wat(module) => {
                let bytes = module.encode().map_err(|e| format!("{e}"))?;
                self.instantiate(&bytes).map(|_| Vec::new())
            }
        }
    }

    fn define(&mut self, module: &mut QuoteWat<'_>) -> Result<(), String> {
        let name = module.name().map(|id| String::from(id.name()));
        let bytes = module.encode().map_err(|e| format!("{e}"))?;
        let instance = self.instantiate(&bytes)?;
        self.last = Some(instance);
        if let Some(name) = name {
            self.named.insert(name, instance);
        }
        Ok(())
    }

    fn run(&mut self, directive: &mut WastDirective<'_>) -> Result<(), String> {
        match directive {
            WastDirective::Module(module) => self.define(module),
            WastDirective::Register { name, module, .. } => {
                let instance = self.instance(*module)?;
                self.linker
                    .instance(&mut self.store, name, instance)
                    .map(|_| ())
                    .map_err(|e| format!("{e}"))
            }
            WastDirective::Invoke(invoke) => self.invoke(invoke).map(|_| ()),
            WastDirective::AssertReturn { exec, results, .. } => {
                let actual = self.execute(exec)?;
                compare(&actual, results)
            }
            WastDirective::AssertTrap { exec, .. } => match self.execute(exec) {
                Err(error) if error.starts_with("trap") => Ok(()),
                Err(error) => Err(error),
                Ok(_) => Err("expected a trap".into()),
            },
            // TRust: the exception-handling scripts (`test/core/exceptions`).
            WastDirective::AssertException { exec, .. } => match self.execute(exec) {
                Err(error) if error.contains("uncaught WebAssembly exception") => Ok(()),
                Err(error) => Err(error),
                Ok(_) => Err("expected an exception".into()),
            },
            WastDirective::AssertInvalid { module, .. }
            | WastDirective::AssertMalformed { module, .. } => match module.encode() {
                Err(_) => Ok(()),
                Ok(bytes) => match Module::new(self.store.engine(), &bytes[..]) {
                    Err(_) => Ok(()),
                    Ok(_) => Err("module unexpectedly validated".into()),
                },
            },
            WastDirective::AssertUnlinkable { module, .. } => {
                let bytes = module.encode().map_err(|e| format!("{e}"))?;
                match self.instantiate(&bytes) {
                    Err(_) => Ok(()),
                    Ok(_) => Err("module unexpectedly linked".into()),
                }
            }
            _ => Err("unsupported directive".into()),
        }
    }
}

fn argument(arg: &WastArg<'_>) -> Result<Val, String> {
    Ok(match arg {
        WastArg::Core(WastArgCore::I32(value)) => Val::I32(*value),
        WastArg::Core(WastArgCore::I64(value)) => Val::I64(*value),
        WastArg::Core(WastArgCore::F32(value)) => Val::F32(f32::from_bits(value.bits).into()),
        WastArg::Core(WastArgCore::F64(value)) => Val::F64(f64::from_bits(value.bits).into()),
        WastArg::Core(WastArgCore::V128(value)) => {
            Val::V128(V128::from(u128::from_le_bytes(value.to_le_bytes())))
        }
        _ => return Err("unsupported argument".into()),
    })
}

fn f32_matches(actual: u32, expected: &NanPattern<wast::token::F32>) -> bool {
    match expected {
        NanPattern::Value(expected) => actual == expected.bits,
        NanPattern::CanonicalNan => actual & 0x7fff_ffff == 0x7fc0_0000,
        NanPattern::ArithmeticNan => actual & 0x7fc0_0000 == 0x7fc0_0000,
    }
}

fn f64_matches(actual: u64, expected: &NanPattern<wast::token::F64>) -> bool {
    match expected {
        NanPattern::Value(expected) => actual == expected.bits,
        NanPattern::CanonicalNan => actual & 0x7fff_ffff_ffff_ffff == 0x7ff8_0000_0000_0000,
        NanPattern::ArithmeticNan => actual & 0x7ff8_0000_0000_0000 == 0x7ff8_0000_0000_0000,
    }
}

fn v128_matches(actual: u128, expected: &V128Pattern) -> bool {
    let bytes = actual.to_le_bytes();
    let lanes = |width: usize| {
        bytes
            .chunks(width)
            .map(|lane| {
                let mut word = [0_u8; 8];
                word[..width].copy_from_slice(lane);
                u64::from_le_bytes(word)
            })
            .collect::<Vec<_>>()
    };
    match expected {
        V128Pattern::I8x16(expected) => expected
            .iter()
            .zip(lanes(1))
            .all(|(e, a)| u64::from(*e as u8) == a),
        V128Pattern::I16x8(expected) => expected
            .iter()
            .zip(lanes(2))
            .all(|(e, a)| u64::from(*e as u16) == a),
        V128Pattern::I32x4(expected) => expected
            .iter()
            .zip(lanes(4))
            .all(|(e, a)| u64::from(*e as u32) == a),
        V128Pattern::I64x2(expected) => expected.iter().zip(lanes(8)).all(|(e, a)| *e as u64 == a),
        V128Pattern::F32x4(expected) => expected
            .iter()
            .zip(lanes(4))
            .all(|(e, a)| f32_matches(a as u32, e)),
        V128Pattern::F64x2(expected) => expected
            .iter()
            .zip(lanes(8))
            .all(|(e, a)| f64_matches(a, e)),
    }
}

fn core_matches(actual: &Val, expected: &WastRetCore<'_>) -> bool {
    match (actual, expected) {
        (Val::I32(a), WastRetCore::I32(e)) => a == e,
        (Val::I64(a), WastRetCore::I64(e)) => a == e,
        (Val::F32(a), WastRetCore::F32(e)) => f32_matches(a.to_bits(), e),
        (Val::F64(a), WastRetCore::F64(e)) => f64_matches(a.to_bits(), e),
        (Val::V128(a), WastRetCore::V128(e)) => v128_matches(a.as_u128(), e),
        (_, WastRetCore::Either(options)) => options.iter().any(|e| core_matches(actual, e)),
        _ => false,
    }
}

fn compare(actual: &[Val], expected: &[WastRet<'_>]) -> Result<(), String> {
    if actual.len() != expected.len() {
        return Err("result arity mismatch".into());
    }
    for (actual, expected) in actual.iter().zip(expected) {
        let matched = match expected {
            WastRet::Core(expected) => core_matches(actual, expected),
            _ => false,
        };
        if !matched {
            return Err(format!("expected {expected:?}, got {actual:?}"));
        }
    }
    Ok(())
}

struct Outcome {
    passed: usize,
    failures: Vec<String>,
    regions: usize,
}

fn run_file(path: &Path, native: bool) -> Outcome {
    let source = std::fs::read_to_string(path).unwrap();
    let mut outcome = Outcome {
        passed: 0,
        failures: Vec::new(),
        regions: 0,
    };
    // Scripts the text parser rejects fail identically in both modes.
    let Ok(buffer) = ParseBuffer::new(&source) else {
        outcome.failures.push(format!("{}: unparsable", path.display()));
        return outcome;
    };
    let Ok(mut wast) = parser::parse::<Wast<'_>>(&buffer) else {
        outcome.failures.push(format!("{}: unparsable", path.display()));
        return outcome;
    };
    let mut runner = Runner::new(native);
    for directive in &mut wast.directives {
        let (line, _) = directive.span().linecol_in(&source);
        match runner.run(directive) {
            Ok(()) => outcome.passed += 1,
            Err(error) => {
                outcome
                    .failures
                    .push(format!("{}:{}: {error}", path.display(), line + 1))
            }
        }
    }
    outcome.regions = super::tests::compiled_regions(runner.store.engine());
    outcome
}

#[test]
#[ignore = "requires WASMI_SPEC_DIR pointing at the official test/core/simd directory"]
fn native_simd_spec_scripts_match_interpreter() {
    let dir = std::env::var("WASMI_SPEC_DIR").expect("WASMI_SPEC_DIR");
    // `WASMI_SPEC_PREFIX` selects other scripts, for example `""` for every core script.
    let prefix = std::env::var("WASMI_SPEC_PREFIX").unwrap_or_else(|_| String::from("simd_"));
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&*prefix) && name.ends_with(".wast"))
        })
        .collect();
    paths.sort();
    let mut regressions = Vec::new();
    let (mut passed, mut failed, mut regions) = (0, 0, 0);
    for path in &paths {
        let interpreted = run_file(path, false);
        let native = run_file(path, true);
        std::eprintln!(
            "{}: interpreter {} passed / {} failed; native {} passed / {} failed, {} regions",
            path.file_name().unwrap().to_string_lossy(),
            interpreted.passed,
            interpreted.failures.len(),
            native.passed,
            native.failures.len(),
            native.regions,
        );
        for failure in &interpreted.failures {
            std::eprintln!("  interpreter: {failure}");
        }
        // Compare failing directives by location: messages embed store identities.
        let location = |failure: &String| {
            failure
                .splitn(3, ':')
                .take(2)
                .collect::<Vec<_>>()
                .join(":")
        };
        for failure in &native.failures {
            if !interpreted
                .failures
                .iter()
                .any(|interpreted| location(interpreted) == location(failure))
            {
                regressions.push(failure.clone());
            }
        }
        passed += native.passed;
        failed += native.failures.len();
        regions += native.regions;
    }
    std::eprintln!(
        "{} files: native {passed} passed, {failed} failed, {regions} regions; {} native-only failures",
        paths.len(),
        regressions.len()
    );
    for regression in &regressions {
        std::eprintln!("  native only: {regression}");
    }
    assert!(regressions.is_empty());
}
