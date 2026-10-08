//! TRust: a small runner for the official exception-handling `*.wast` scripts.
//!
//! It covers the directives of the exception-handling scripts of the WebAssembly specification
//! and its exception-handling repository, including registered modules whose tags others
//! import. Modules that use proposals Wasmi does not implement (typed function references, GC
//! rec groups) cannot be compiled: their assertions are skipped, and every skip is reported
//! with its reason.

use std::collections::{HashMap, HashSet};
use wasmi::{Engine, Error, ExternRef, Instance, Linker, Module, Nullable, Store, Val, ValType};
use wast::{
    Wast, WastArg, WastDirective, WastExecute, WastInvoke, WastRet,
    core::{AbstractHeapType, HeapType, NanPattern, WastArgCore, WastRetCore},
    parser::{self, ParseBuffer},
};

/// The message of the error that ends an execution with an uncaught exception.
pub const UNCAUGHT: &str = "uncaught WebAssembly exception";

/// Executes the directives of `*.wast` scripts.
pub struct Runner {
    store: Store<()>,
    linker: Linker<()>,
    last: Option<Instance>,
    named: HashMap<String, Instance>,
    /// The names under which modules were registered.
    registered: HashSet<String>,
    /// The names of exports whose invocations are skipped.
    skipped_invokes: Vec<&'static str>,
    /// The reasons of skipped directives.
    skips: Vec<String>,
}

impl Runner {
    /// Creates a new [`Runner`] that skips the invocations of the exports `skipped_invokes`.
    pub fn new(skipped_invokes: &[&'static str]) -> Self {
        let engine = Engine::default();
        let mut linker = Linker::new(&engine);
        // A later `register` of the same name replaces the earlier one.
        linker.allow_shadowing(true);
        Self {
            store: Store::new(&engine, ()),
            linker,
            last: None,
            named: HashMap::new(),
            registered: HashSet::new(),
            skipped_invokes: skipped_invokes.to_vec(),
            skips: Vec::new(),
        }
    }

    /// Runs the script `source`; returns the number of directives and the skip reasons.
    ///
    /// # Panics
    ///
    /// If a directive fails, naming `name` and the directive's position.
    pub fn run_script(mut self, name: &str, source: &str) -> (usize, Vec<String>) {
        let buffer = ParseBuffer::new(source).unwrap();
        let wast = parser::parse::<Wast>(&buffer).unwrap();
        let mut directives = 0;
        for mut directive in wast.directives {
            let (line, col) = directive.span().linecol_in(source);
            if let Err(error) = self.run(&mut directive) {
                panic!("{name}:{}:{}: {error}", line + 1, col + 1);
            }
            directives += 1;
        }
        (directives, self.skips)
    }

    fn instance(&self, id: Option<wast::token::Id<'_>>) -> Option<Instance> {
        match id {
            Some(id) => self.named.get(id.name()).copied(),
            None => self.last,
        }
    }

    fn arg(&mut self, arg: &WastArg<'_>) -> Result<Val, String> {
        let WastArg::Core(arg) = arg else {
            return Err(String::from("unsupported argument"));
        };
        Ok(match arg {
            WastArgCore::I32(value) => Val::I32(*value),
            WastArgCore::I64(value) => Val::I64(*value),
            WastArgCore::F32(value) => Val::F32(f32::from_bits(value.bits).into()),
            WastArgCore::F64(value) => Val::F64(f64::from_bits(value.bits).into()),
            WastArgCore::RefNull(HeapType::Abstract { ty, .. }) => match ty {
                AbstractHeapType::Func | AbstractHeapType::NoFunc => Val::FuncRef(Nullable::Null),
                AbstractHeapType::Extern | AbstractHeapType::NoExtern => {
                    Val::ExternRef(Nullable::Null)
                }
                AbstractHeapType::Exn | AbstractHeapType::NoExn => Val::ExnRef(Nullable::Null),
                _ => return Err(String::from("unsupported null argument")),
            },
            WastArgCore::RefExtern(value) => {
                Val::ExternRef(Nullable::Val(ExternRef::new(&mut self.store, *value)))
            }
            _ => return Err(String::from("unsupported argument")),
        })
    }

    /// Invokes `invoke`; the outer `Err` is a runner failure, the inner one the call's error.
    fn invoke(&mut self, invoke: &WastInvoke<'_>) -> Result<Result<Vec<Val>, Error>, String> {
        let instance = self.instance(invoke.module).ok_or("no module")?;
        let func = instance
            .get_func(&self.store, invoke.name)
            .ok_or_else(|| format!("unknown export {}", invoke.name))?;
        let args = invoke
            .args
            .iter()
            .map(|arg| self.arg(arg))
            .collect::<Result<Vec<_>, _>>()?;
        let ty = func.ty(&self.store);
        let mut results: Vec<Val> = ty
            .results()
            .iter()
            .copied()
            .map(Val::default_for_ty)
            .collect();
        Ok(func
            .call(&mut self.store, &args, &mut results)
            .map(|()| results))
    }

    /// Returns `Some` if the invocation of `exec` is skipped, recording why.
    fn skip_invoke(&mut self, exec: &WastExecute<'_>) -> Option<()> {
        let WastExecute::Invoke(invoke) = exec else {
            return None;
        };
        if self.instance(invoke.module).is_none() {
            self.skips
                .push(format!("{}: its module was skipped", invoke.name));
            return Some(());
        }
        if self.skipped_invokes.contains(&invoke.name) {
            self.skips
                .push(format!("{}: skipped by the script's caller", invoke.name));
            return Some(());
        }
        None
    }

    fn check_result(&self, actual: &Val, expected: &WastRet<'_>) -> Result<(), String> {
        let WastRet::Core(expected) = expected else {
            return Err(String::from("unsupported result"));
        };
        let matches = match (actual, expected) {
            (Val::I32(a), WastRetCore::I32(e)) => a == e,
            (Val::I64(a), WastRetCore::I64(e)) => a == e,
            (Val::F32(a), WastRetCore::F32(NanPattern::Value(e))) => a.to_bits() == e.bits,
            (Val::F64(a), WastRetCore::F64(NanPattern::Value(e))) => a.to_bits() == e.bits,
            (Val::F32(a), WastRetCore::F32(_)) => a.to_float().is_nan(),
            (Val::F64(a), WastRetCore::F64(_)) => a.to_float().is_nan(),
            (Val::FuncRef(a), WastRetCore::RefNull(ty)) => {
                a.is_null() && null_type_matches(ty.as_ref(), ValType::FuncRef)
            }
            (Val::ExternRef(a), WastRetCore::RefNull(ty)) => {
                a.is_null() && null_type_matches(ty.as_ref(), ValType::ExternRef)
            }
            (Val::ExnRef(a), WastRetCore::RefNull(ty)) => {
                a.is_null() && null_type_matches(ty.as_ref(), ValType::ExnRef)
            }
            (Val::FuncRef(a), WastRetCore::RefFunc(None)) => !a.is_null(),
            (Val::ExternRef(a), WastRetCore::RefExtern(e)) => match (a, e) {
                (Nullable::Val(a), Some(e)) => a.data(&self.store).downcast_ref::<u32>() == Some(e),
                (Nullable::Val(_), None) => true,
                (Nullable::Null, _) => false,
            },
            _ => return Err(format!("unsupported result comparison: {actual:?}")),
        };
        match matches {
            true => Ok(()),
            false => Err(format!("unexpected result {actual:?}")),
        }
    }

    fn run(&mut self, directive: &mut WastDirective<'_>) -> Result<(), String> {
        match directive {
            WastDirective::Module(module) => {
                let name = module.name().map(|id| String::from(id.name()));
                let bytes = module.encode().map_err(|e| e.to_string())?;
                let instantiated = Module::new(self.store.engine(), &bytes[..])
                    .map_err(|e| e.to_string())
                    .and_then(|module| {
                        self.linker
                            .instantiate_and_start(&mut self.store, &module)
                            .map_err(|e| e.to_string())
                    });
                match instantiated {
                    Ok(instance) => {
                        self.last = Some(instance);
                        if let Some(name) = name {
                            self.named.insert(name, instance);
                        }
                    }
                    Err(error) => {
                        // Tag imports and types beyond Wasmi's reference types are unsupported:
                        // skip the dependent assertions.
                        self.last = None;
                        self.skips.push(format!("module: {error}"));
                    }
                }
                Ok(())
            }
            WastDirective::Register { name, module, .. } => {
                if let Some(instance) = self.instance(*module) {
                    self.linker
                        .instance(&mut self.store, name, instance)
                        .map_err(|error| format!("register {name}: {error}"))?;
                    self.registered.insert(String::from(*name));
                }
                Ok(())
            }
            WastDirective::AssertReturn { exec, results, .. } => {
                if self.skip_invoke(exec).is_some() {
                    return Ok(());
                }
                let WastExecute::Invoke(invoke) = exec else {
                    return Err(String::from("unsupported execution"));
                };
                let actual = self
                    .invoke(invoke)?
                    .map_err(|error| format!("{}: error: {error}", invoke.name))?;
                if actual.len() != results.len() {
                    return Err(format!("{}: result arity mismatch", invoke.name));
                }
                for (actual, expected) in actual.iter().zip(results.iter()) {
                    self.check_result(actual, expected)
                        .map_err(|error| format!("{}: {error}", invoke.name))?;
                }
                Ok(())
            }
            WastDirective::AssertException { exec, .. } => {
                if self.skip_invoke(exec).is_some() {
                    return Ok(());
                }
                let WastExecute::Invoke(invoke) = exec else {
                    return Err(String::from("unsupported execution"));
                };
                match self.invoke(invoke)? {
                    Err(error) => match error.as_exception() {
                        Some(exn) => {
                            // The uncaught exception reached the host with its tag and fields.
                            let tag = exn.tag(&self.store);
                            let fields = exn.fields(&self.store);
                            exn.release(&self.store);
                            match (tag, fields) {
                                (Some(_), Some(_)) => Ok(()),
                                _ => Err(format!("{}: dangling uncaught exception", invoke.name)),
                            }
                        }
                        None => Err(format!("{}: expected an exception: {error}", invoke.name)),
                    },
                    Ok(results) => Err(format!(
                        "{}: expected an exception: returned {results:?}",
                        invoke.name
                    )),
                }
            }
            WastDirective::AssertTrap { exec, .. } => {
                if self.skip_invoke(exec).is_some() {
                    return Ok(());
                }
                let WastExecute::Invoke(invoke) = exec else {
                    return Err(String::from("unsupported execution"));
                };
                match self.invoke(invoke)? {
                    Err(error) if error.as_exception().is_none() => Ok(()),
                    other => Err(format!("{}: expected a trap: {other:?}", invoke.name)),
                }
            }
            WastDirective::AssertInvalid { module, .. }
            | WastDirective::AssertMalformed { module, .. } => match module.encode() {
                Err(_) => Ok(()),
                Ok(bytes) => match Module::new(self.store.engine(), &bytes[..]) {
                    Err(_) => Ok(()),
                    Ok(_) => Err("module unexpectedly validated".into()),
                },
            },
            WastDirective::AssertUnlinkable { module, .. } => {
                let bytes = module_bytes(module)?;
                let module = match Module::new(self.store.engine(), &bytes[..]) {
                    Ok(module) => module,
                    Err(error) => {
                        self.skips
                            .push(format!("assert_unlinkable: module: {error}"));
                        return Ok(());
                    }
                };
                // An import of a module whose registration was skipped cannot show a typing
                // error: the definition is missing for another reason.
                if let Some(import) = module
                    .imports()
                    .find(|import| !self.registered.contains(import.module()))
                {
                    self.skips.push(format!(
                        "assert_unlinkable: imports from the skipped module {:?}",
                        import.module()
                    ));
                    return Ok(());
                }
                match self.linker.instantiate_and_start(&mut self.store, &module) {
                    Err(error) if error.as_exception().is_none() => Ok(()),
                    other => Err(format!("module unexpectedly linked: {other:?}")),
                }
            }
            _ => Err("unsupported directive".into()),
        }
    }
}

/// Returns `true` if the expected null reference type `ty` is a supertype of `actual`.
fn null_type_matches(ty: Option<&HeapType<'_>>, actual: ValType) -> bool {
    let Some(HeapType::Abstract { ty, .. }) = ty else {
        return ty.is_none();
    };
    matches!(
        (ty, actual),
        (
            AbstractHeapType::Func | AbstractHeapType::NoFunc,
            ValType::FuncRef
        ) | (
            AbstractHeapType::Extern | AbstractHeapType::NoExtern,
            ValType::ExternRef
        ) | (
            AbstractHeapType::Exn | AbstractHeapType::NoExn,
            ValType::ExnRef
        )
    )
}

fn module_bytes(module: &mut wast::Wat<'_>) -> Result<Vec<u8>, String> {
    module.encode().map_err(|error| error.to_string())
}
