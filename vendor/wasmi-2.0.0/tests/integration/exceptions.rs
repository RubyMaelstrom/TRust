//! TRust: WebAssembly exception handling (`try_table`, `throw`, `throw_ref` and `exnref`).
//!
//! WebAssembly Core 3.0, `exec/instructions.rst` (`exec-try_table`, `exec-throw`,
//! `exec-throw_ref`) and `valid/instructions.rst` (`valid-try_table`, `valid-catch`; local
//! snapshots of the spec repository 37d6b059 and the exception-handling repository af287a73):
//! a `try_table` installs a handler for its body whose catch clauses are tested in order and
//! branch to labels counted from outside the `try_table`; catch clauses with `_ref` also push
//! the exception reference, which `throw_ref` raises again.
//!
//! `WASMI_EH_DIR=/path/to/spec/test/core/exceptions cargo test exception_spec -- --ignored`
//! runs the official scripts. Modules that import tags are skipped: Wasmi has no tag externs.

use super::wast_script::{Runner, UNCAUGHT};
use std::path::Path;
use wasmi::{Engine, Func, FuncType, Instance, Linker, Module, Nullable, Store, Val, ValType};

fn instantiate(wat: &str) -> (Store<()>, Instance) {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let module = Module::new(&engine, wat::parse_str(wat).unwrap()).unwrap();
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    (store, instance)
}

fn call(
    store: &mut Store<()>,
    instance: Instance,
    name: &str,
    args: &[Val],
) -> Result<Vec<Val>, String> {
    let func = instance.get_func(&*store, name).unwrap();
    let ty = func.ty(&*store);
    let mut results: Vec<Val> = ty
        .results()
        .iter()
        .copied()
        .map(Val::default_for_ty)
        .collect();
    func.call(&mut *store, args, &mut results)
        .map_err(|error| error.to_string())?;
    Ok(results)
}

fn call_i32(
    store: &mut Store<()>,
    instance: Instance,
    name: &str,
    args: &[Val],
) -> Result<i32, String> {
    let results = call(store, instance, name, args)?;
    Ok(results[0].i32().unwrap())
}

#[test]
fn catch_clauses_branch_with_tag_fields() {
    // The fields fill the label's results, the last of which the translator expects in
    // accumulator registers (`exec-throw_ref` step 15c).
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32 i64 f32 f64))
          (func $throw (param i32)
            (throw $e (local.get 0) (i64.const -2) (f32.const 1.5) (f64.const 2.25)))
          (func (export "run") (param i32) (result i32 i64 f32 f64)
            (block $h (result i32 i64 f32 f64)
              (try_table (catch $e $h)
                (call $throw (local.get 0)))
              (unreachable))))
        "#,
    );
    let results = call(&mut store, instance, "run", &[Val::I32(9)]).unwrap();
    assert_eq!(results[0].i32(), Some(9));
    assert_eq!(results[1].i64(), Some(-2));
    assert_eq!(results[2].f32().unwrap().to_float(), 1.5);
    assert_eq!(results[3].f64().unwrap().to_float(), 2.25);
}

#[test]
fn catch_clauses_carry_v128_fields_and_references() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $v (param v128 i32))
          (func $throw (param i32)
            (throw $v (i32x4.splat (local.get 0)) (i32.const 3)))
          (func (export "run") (param i32) (result i32)
            (local $exn exnref)
            (local $sum i32)
            block $h (result v128 i32 exnref)
              try_table (catch_ref $v $h)
                local.get 0
                call $throw
              end
              unreachable
            end
            local.set $exn
            local.set $sum
            i32x4.extract_lane 2
            local.get $sum
            i32.add
            local.set $sum
            block $h2 (result v128 i32)
              try_table (catch $v $h2)
                local.get $exn
                throw_ref
              end
              unreachable
            end
            local.get $sum
            i32.add
            local.set $sum
            i32x4.extract_lane 1
            local.get $sum
            i32.add))
        "#,
    );
    // Both clauses see the lanes and the field: 11 + 3 + 3 + 11.
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(11)]),
        Ok(28)
    );
}

#[test]
fn catch_labels_include_the_function_body_and_loops() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (func (export "to_return") (result i32)
            ;; Label 0 of the clause is the function body: the branch returns.
            (try_table (catch $e 0)
              (throw $e (i32.const 7)))
            (i32.const -1))
          (func (export "to_loop") (param $n i32) (result i32)
            (local $sum i32)
            (local.get $n)
            (loop $l (param i32)
              (local.set $n)
              (if (i32.eqz (local.get $n)) (then (return (local.get $sum))))
              (local.set $sum (i32.add (local.get $sum) (local.get $n)))
              ;; The clause re-enters the loop with the field as its parameter.
              (try_table (catch $e $l)
                (throw $e (i32.sub (local.get $n) (i32.const 1)))))
            (unreachable)))
        "#,
    );
    assert_eq!(call_i32(&mut store, instance, "to_return", &[]), Ok(7));
    assert_eq!(
        call_i32(&mut store, instance, "to_loop", &[Val::I32(100)]),
        Ok(5050)
    );
}

#[test]
fn handlers_preserve_operands_and_local_writes() {
    // The operands below a `try_table` survive the exceptional edge, including a local's old
    // value and a temporary held in a register, and the locals keep writes made before the throw.
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e)
          (func $throw (throw $e))
          (func (export "run") (param $x i32) (result i32)
            (local $y i32)
            i32.const 10
            local.set $y
            local.get $x
            local.get $y
            local.get $x
            i32.const 5
            i32.add
            block $h
              try_table (catch_all $h)
                i32.const 1000
                local.set $x
                i32.const 2000
                local.set $y
                call $throw
              end
            end
            i32.add
            i32.add
            local.get $x
            i32.add
            local.get $y
            i32.add))
        "#,
    );
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(1)]),
        Ok(3017)
    );
}

#[test]
fn clauses_are_tested_in_order_and_unmatched_exceptions_continue() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $a (param i32))
          (tag $b (param i32))
          (tag $c)
          (func $throw (param i32)
            (if (i32.eqz (local.get 0)) (then (throw $a (i32.const 10))))
            (if (i32.eq (local.get 0) (i32.const 1)) (then (throw $b (i32.const 20))))
            (throw $c))
          (func $middle (param i32) (result i32)
            ;; A `try_table` without a matching clause passes the exception on.
            (block $h (result i32)
              (try_table (result i32) (catch $a $h)
                (try_table (result i32)
                  (call $throw (local.get 0))
                  (i32.const -1)))
              (return (i32.const -2)))
            (i32.const 100)
            (i32.add))
          (func (export "run") (param i32) (result i32)
            (block $all
              (block $hb (result i32)
                (try_table (result i32) (catch $b $hb) (catch_all $all) (catch $c $all)
                  (call $middle (local.get 0)))
                (return))
              (return (i32.add (i32.const 200))))
            (i32.const 300)))
        "#,
    );
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(0)]),
        Ok(110)
    );
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(1)]),
        Ok(220)
    );
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(2)]),
        Ok(300)
    );
}

#[test]
fn try_table_parameters_and_results() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (func (export "run") (param i32) (result i32)
            (block $h (result i32)
              (local.get 0)
              (try_table (param i32) (result i32) (catch $e $h)
                (if (param i32) (result i32) (i32.eqz (local.get 0))
                  (then (throw $e))
                  (else (i32.const 1) (i32.add)))))
            (i32.const 1000)
            (i32.add)))
        "#,
    );
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(0)]),
        Ok(1000)
    );
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(4)]),
        Ok(1005)
    );
}

#[test]
fn one_exception_passes_legacy_and_standard_handlers() {
    // `throw`, a legacy `catch_all` with `rethrow`, `catch_all_ref`, `throw_ref`, a legacy
    // `catch` with `rethrow` and a `try_table` `catch` all see the same exception.
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (func $throw (param i32) (throw $e (local.get 0)))
          (func (export "run") (param i32) (result i32)
            block $outer (result i32)
              try_table (catch $e $outer)
                try
                  block $h (result exnref)
                    try_table (catch_all_ref $h)
                      try
                        local.get 0
                        call $throw
                      catch_all
                        rethrow 0
                      end
                    end
                    unreachable
                  end
                  throw_ref
                catch $e
                  drop
                  rethrow 0
                end
              end
              i32.const -1
            end))
        "#,
    );
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(42)]),
        Ok(42)
    );
}

#[test]
fn legacy_delegate_to_a_try_table() {
    // The handler of a `try_table` applies to exceptions raised in its body, so a `delegate`
    // to its label hands the exception to its clauses.
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (func $throw (param i32) (throw $e (local.get 0)))
          (func (export "run") (param i32) (result i32)
            block $h (result i32)
              try_table (catch $e $h)
                block
                  try
                    local.get 0
                    call $throw
                  delegate 1
                end
              end
              i32.const -1
            end))
        "#,
    );
    assert_eq!(call_i32(&mut store, instance, "run", &[Val::I32(5)]), Ok(5));
}

#[test]
fn exception_references_are_values() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (global $g (mut exnref) (ref.null exn))
          (table $t 2 exnref)
          (func $catch (param i32) (result exnref)
            (block $h (result exnref)
              (try_table (catch_all_ref $h) (throw $e (local.get 0)))
              (unreachable)))
          (func $payload (param exnref) (result i32)
            (block $h (result i32)
              (try_table (catch $e $h) (throw_ref (local.get 0)))
              (unreachable)))
          (func (export "store") (param i32)
            (global.set $g (call $catch (local.get 0)))
            (table.set $t (i32.const 1) (call $catch (i32.add (local.get 0) (i32.const 1)))))
          (func (export "from_global") (result i32) (call $payload (global.get $g)))
          (func (export "from_table") (param i32) (result i32)
            (call $payload (table.get $t (local.get 0))))
          (func (export "is_null") (param i32) (result i32)
            (ref.is_null (table.get $t (local.get 0))))
          (func (export "select") (param i32) (result i32)
            (call $payload
              (select (result exnref) (global.get $g) (table.get $t (i32.const 1)) (local.get 0))))
          (func (export "grow") (result i32) (table.grow $t (global.get $g) (i32.const 3)))
          (func (export "fill_copy")
            (table.fill $t (i32.const 0) (table.get $t (i32.const 1)) (i32.const 1))
            (table.copy $t $t (i32.const 4) (i32.const 0) (i32.const 1)))
          (func (export "get") (result exnref) (global.get $g))
          (func (export "throw") (param exnref) (throw_ref (local.get 0)))
          (func (export "throw_null") (throw_ref (ref.null exn)))
          (func (export "throw_null_dyn") (param i32)
            (throw_ref (select (result exnref) (ref.null exn) (global.get $g) (local.get 0)))))
        "#,
    );
    call(&mut store, instance, "store", &[Val::I32(5)]).unwrap();
    assert_eq!(call_i32(&mut store, instance, "from_global", &[]), Ok(5));
    assert_eq!(
        call_i32(&mut store, instance, "from_table", &[Val::I32(1)]),
        Ok(6)
    );
    assert_eq!(
        call_i32(&mut store, instance, "is_null", &[Val::I32(0)]),
        Ok(1)
    );
    assert_eq!(
        call_i32(&mut store, instance, "is_null", &[Val::I32(1)]),
        Ok(0)
    );
    assert_eq!(
        call_i32(&mut store, instance, "select", &[Val::I32(1)]),
        Ok(5)
    );
    assert_eq!(
        call_i32(&mut store, instance, "select", &[Val::I32(0)]),
        Ok(6)
    );
    assert_eq!(call_i32(&mut store, instance, "grow", &[]), Ok(2));
    assert_eq!(
        call_i32(&mut store, instance, "from_table", &[Val::I32(4)]),
        Ok(5)
    );
    call(&mut store, instance, "fill_copy", &[]).unwrap();
    assert_eq!(
        call_i32(&mut store, instance, "from_table", &[Val::I32(0)]),
        Ok(6)
    );
    assert_eq!(
        call_i32(&mut store, instance, "from_table", &[Val::I32(4)]),
        Ok(6)
    );
    // `throw_ref` traps on null (`exec-throw_ref` step 3) and the trap is not an exception.
    let error = call(&mut store, instance, "throw_null", &[]).unwrap_err();
    assert!(error.contains("null exception reference"), "{error}");
    let error = call(&mut store, instance, "throw_null_dyn", &[Val::I32(1)]).unwrap_err();
    assert!(error.contains("null exception reference"), "{error}");
    let error = call(&mut store, instance, "throw_null_dyn", &[Val::I32(0)]).unwrap_err();
    assert!(error.contains(UNCAUGHT), "{error}");
    // The host receives exception references and passes them back.
    let exn = call(&mut store, instance, "get", &[]).unwrap().remove(0);
    assert!(matches!(exn, Val::ExnRef(Nullable::Val(_))));
    let error = call(&mut store, instance, "throw", &[exn]).unwrap_err();
    assert!(error.contains(UNCAUGHT), "{error}");
    let error = call(
        &mut store,
        instance,
        "throw",
        &[Val::ExnRef(Nullable::Null)],
    )
    .unwrap_err();
    assert!(error.contains("null exception reference"), "{error}");
}

#[test]
fn exceptions_carry_exception_references() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $inner (param i32))
          (tag $outer (param exnref))
          (func (export "run") (param i32) (result i32)
            (block $h2 (result i32)
              (try_table (catch $inner $h2)
                (block $h1 (result exnref)
                  (try_table (catch $outer $h1)
                    (block $h0 (result exnref)
                      (try_table (catch_all_ref $h0) (throw $inner (local.get 0)))
                      (unreachable))
                    (throw $outer))
                  (unreachable))
                (throw_ref))
              (unreachable))))
        "#,
    );
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(77)]),
        Ok(77)
    );
}

#[test]
fn many_caught_references_stay_valid() {
    // Enough exceptions are caught by reference for the store to collect several times: the
    // references in a table, a global, a payload and those held by the host stay valid.
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (tag $box (param exnref))
          (global $g (mut exnref) (ref.null exn))
          (table $t 4 exnref)
          (func $catch (param i32) (result exnref)
            (block $h (result exnref)
              (try_table (catch_all_ref $h) (throw $e (local.get 0)))
              (unreachable)))
          (func $box (param exnref) (result exnref)
            (block $h (result exnref)
              (try_table (catch_all_ref $h) (throw $box (local.get 0)))
              (unreachable)))
          (func $payload (param exnref) (result i32)
            (block $h (result i32)
              (try_table (catch $e $h) (throw_ref (local.get 0)))
              (unreachable)))
          (func $unbox (param exnref) (result exnref)
            (block $h (result exnref)
              (try_table (catch $box $h) (throw_ref (local.get 0)))
              (unreachable)))
          (func (export "churn") (param $n i32)
            (local $i i32)
            (local $keep exnref)
            (local.set $keep (call $box (call $catch (i32.const -7))))
            (loop $l
              (table.set $t (i32.and (local.get $i) (i32.const 3)) (call $catch (local.get $i)))
              (if (i32.eqz (i32.rem_u (local.get $i) (i32.const 1000)))
                (then (global.set $g (call $catch (local.get $i)))))
              (drop (call $catch (local.get $i)))
              (local.set $i (i32.add (local.get $i) (i32.const 1)))
              (br_if $l (i32.lt_u (local.get $i) (local.get $n))))
            (if (i32.ne (call $payload (call $unbox (local.get $keep))) (i32.const -7))
              (then (unreachable))))
          (func (export "slot") (param i32) (result i32)
            (call $payload (table.get $t (local.get 0))))
          (func (export "global") (result i32) (call $payload (global.get $g)))
          (func (export "make") (param i32) (result exnref) (call $catch (local.get 0)))
          (func (export "payload") (param exnref) (result i32) (call $payload (local.get 0))))
        "#,
    );
    let held = call(&mut store, instance, "make", &[Val::I32(1234)])
        .unwrap()
        .remove(0);
    call(&mut store, instance, "churn", &[Val::I32(20_000)]).unwrap();
    for slot in 0..4 {
        assert_eq!(
            call_i32(&mut store, instance, "slot", &[Val::I32(slot)]),
            Ok(19_996 + slot)
        );
    }
    assert_eq!(call_i32(&mut store, instance, "global", &[]), Ok(19_000));
    call(&mut store, instance, "churn", &[Val::I32(5_000)]).unwrap();
    assert_eq!(call_i32(&mut store, instance, "payload", &[held]), Ok(1234));
}

#[test]
fn host_functions_receive_and_return_exception_references() {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = <Linker<()>>::new(&engine);
    let ty = FuncType::new([ValType::ExnRef], [ValType::ExnRef]);
    let identity = Func::new(&mut store, ty, |_caller, params, results| {
        results[0] = params[0].clone();
        Ok(())
    });
    linker.define("host", "identity", identity).unwrap();
    let wasm = wat::parse_str(
        r#"
        (module
          (import "host" "identity" (func $identity (param exnref) (result exnref)))
          (tag $e (param i32))
          (func (export "run") (param i32) (result i32)
            (block $h (result i32)
              (try_table (catch $e $h)
                (block $r (result exnref)
                  (try_table (catch_all_ref $r) (throw $e (local.get 0)))
                  (unreachable))
                (call $identity)
                (throw_ref))
              (unreachable))))
        "#,
    )
    .unwrap();
    let module = Module::new(&engine, wasm).unwrap();
    let instance = linker.instantiate_and_start(&mut store, &module).unwrap();
    assert_eq!(call_i32(&mut store, instance, "run", &[Val::I32(3)]), Ok(3));
}

#[test]
fn suspended_calls_keep_their_exception_references() {
    // A suspended resumable call holds an exception reference on its stack while other calls
    // on the store collect exceptions.
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = <Linker<()>>::new(&engine);
    linker
        .func_wrap("host", "pause", || -> Result<(), wasmi::Error> {
            Err(wasmi::Error::i32_exit(1))
        })
        .unwrap();
    let wasm = wat::parse_str(
        r#"
        (module
          (import "host" "pause" (func $pause))
          (tag $e (param i32))
          (func $catch (param i32) (result exnref)
            (block $h (result exnref)
              (try_table (catch_all_ref $h) (throw $e (local.get 0)))
              (unreachable)))
          (func $payload (param exnref) (result i32)
            (block $h (result i32)
              (try_table (catch $e $h) (throw_ref (local.get 0)))
              (unreachable)))
          (func (export "suspend") (param i32) (result i32)
            (local $x exnref)
            (local.set $x (call $catch (local.get 0)))
            (call $pause)
            (call $payload (local.get $x)))
          (func (export "churn") (param $n i32)
            (loop $l
              (drop (call $catch (local.get $n)))
              (br_if $l (local.tee $n (i32.sub (local.get $n) (i32.const 1)))))))
        "#,
    )
    .unwrap();
    let module = Module::new(&engine, wasm).unwrap();
    let instance = linker.instantiate_and_start(&mut store, &module).unwrap();
    let suspend = instance.get_func(&store, "suspend").unwrap();
    let mut result = [Val::I32(0)];
    let wasmi::ResumableCall::HostTrap(invocation) = suspend
        .call_resumable(&mut store, &[Val::I32(99)], &mut result)
        .unwrap()
    else {
        panic!("expected a suspended call")
    };
    call(&mut store, instance, "churn", &[Val::I32(5_000)]).unwrap();
    let resumed = invocation.resume(&mut store, &[], &mut result).unwrap();
    assert!(matches!(resumed, wasmi::ResumableCall::Finished));
    assert_eq!(result[0].i32(), Some(99));
}

#[test]
fn traps_and_tail_calls_are_not_caught() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e)
          (func $throw (throw $e))
          (func $throw_i32 (result i32) (throw $e))
          (func (export "trap") (result i32)
            (block $h
              (try_table (catch_all $h) (unreachable)))
            (i32.const 1))
          (func (export "tail") (result i32)
            (block $h
              (try_table (result i32) (catch_all $h) (return_call $throw_i32))
              (return))
            (i32.const 1))
          (func (export "uncaught") (throw $e))
          (func (export "after") (result i32)
            (block $h
              (try_table (catch_all $h) (call $throw)))
            (i32.const 2)))
        "#,
    );
    let error = call(&mut store, instance, "trap", &[]).unwrap_err();
    assert!(!error.contains(UNCAUGHT), "{error}");
    let error = call(&mut store, instance, "tail", &[]).unwrap_err();
    assert!(error.contains(UNCAUGHT), "{error}");
    let error = call(&mut store, instance, "uncaught", &[]).unwrap_err();
    assert!(error.contains(UNCAUGHT), "{error}");
    // A following call starts without a stale pending exception.
    assert_eq!(call_i32(&mut store, instance, "after", &[]), Ok(2));
}

#[test]
fn exnref_constant_expressions_and_signatures() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (type $f (func (param exnref) (result exnref)))
          (global $null exnref (ref.null exn))
          (global $none nullexnref (ref.null noexn))
          (table $t 1 exnref)
          (elem $s exnref (ref.null exn) (ref.null noexn))
          (func $id (type $f) (local.get 0))
          (table $fs 1 funcref)
          (elem (table $fs) (i32.const 0) func $id)
          (func (export "run") (result i32)
            (table.init $t $s (i32.const 0) (i32.const 0) (i32.const 1))
            (i32.add
              (ref.is_null (call_indirect $fs (type $f) (global.get $null) (i32.const 0)))
              (ref.is_null (global.get $none)))))
        "#,
    );
    assert_eq!(call_i32(&mut store, instance, "run", &[]), Ok(2));
}

#[test]
fn fuel_metering_charges_catch_clauses() {
    let mut config = wasmi::Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let mut store = Store::new(&engine, ());
    store.set_fuel(1_000_000).unwrap();
    let wasm = wat::parse_str(
        r#"
        (module
          (tag $e (param i32))
          (func (export "run") (param $n i32) (result i32)
            (local $sum i32)
            (loop $l
              (local.set $sum (i32.add (local.get $sum)
                (block $h (result i32)
                  (try_table (catch $e $h)
                    (throw_ref (block $r (result exnref)
                      (try_table (catch_all_ref $r) (throw $e (local.get $n)))
                      (unreachable))))
                  (unreachable))))
              (br_if $l (local.tee $n (i32.sub (local.get $n) (i32.const 1)))))
            (local.get $sum)))
        "#,
    )
    .unwrap();
    let module = Module::new(&engine, wasm).unwrap();
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    assert_eq!(
        call_i32(&mut store, instance, "run", &[Val::I32(100)]),
        Ok(5050)
    );
    let consumed = 1_000_000 - store.get_fuel().unwrap();
    assert!(consumed > 100 * 10, "consumed fuel: {consumed}");
    // Running out of fuel inside a handler is an ordinary trap.
    store.set_fuel(50).unwrap();
    let error = call(&mut store, instance, "run", &[Val::I32(100)]).unwrap_err();
    assert!(!error.contains(UNCAUGHT), "{error}");
}

#[test]
#[ignore]
fn exception_spec() {
    let dir = std::env::var_os("WASMI_EH_DIR").expect("set WASMI_EH_DIR");
    for script in ["throw.wast", "try_table.wast", "throw_ref.wast", "tag.wast"] {
        let path = Path::new(&dir).join(script);
        // Tags cannot be imported: give `try_table.wast`'s main module local tags instead, and
        // skip only the assertions that depend on the identity of the imported tag.
        let source = std::fs::read_to_string(&path)
            .unwrap()
            .replace(
                r#"(tag $imported-e0 (import "test" "e0"))"#,
                "(tag $imported-e0)",
            )
            .replace(
                r#"(tag $imported-e0-alias (import "test" "e0"))"#,
                "(tag $imported-e0-alias)",
            );
        let runner = Runner::new(&["catch-imported", "catch-imported-alias"]);
        let (directives, skips) = runner.run_script(&path.display().to_string(), &source);
        std::println!("{script}: {directives} directives, {} skipped", skips.len());
        for skip in skips {
            std::println!("  skipped {skip}");
        }
    }
}
