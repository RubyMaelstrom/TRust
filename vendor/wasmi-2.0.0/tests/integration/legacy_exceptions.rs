//! TRust: legacy WebAssembly exception handling.
//!
//! Exception handling proposal, `document/legacy/exceptions/core/exec.rst` (local snapshot
//! af287a73): handlers cover only their `try` body, clauses are tested in order, unmatched
//! exceptions continue outward and `rethrow` re-raises the caught exception of an enclosing
//! catch clause, and `delegate` hands it to the `try` at its label.
//!
//! `WASMI_LEGACY_EH_DIR=/path/to/exception-handling/test/legacy/exceptions/core cargo test
//! legacy_exception_spec -- --ignored` runs the official scripts. Modules that import or
//! export tags are skipped: Wasmi has no tag externs.

use super::wast_script::Runner;
use std::path::Path;
use wasmi::{Engine, Instance, Linker, Module, Store, Val};

fn instantiate(wat: &str) -> (Store<()>, Instance) {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let module = Module::new(&engine, wat::parse_str(wat).unwrap()).unwrap();
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    (store, instance)
}

fn call_i32(store: &mut Store<()>, instance: Instance, name: &str, args: &[Val]) -> Result<i32, String> {
    let func = instance.get_func(&*store, name).unwrap();
    let mut results = [Val::I32(0)];
    func.call(&mut *store, args, &mut results)
        .map_err(|error| error.to_string())?;
    Ok(results[0].i32().unwrap())
}

#[test]
fn branch_out_of_try_leaves_its_handler_inactive() {
    // A handler covers its `try` body only: after a `br` leaves the body, a later throw in the
    // same frame must reach the outer handler instead of the stale inner one.
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (func (export "run") (result i32)
            try (result i32)
              block
                try
                  br 1
                catch $e
                  drop
                  i32.const 1
                  return
                end
              end
              i32.const 2
              throw $e
            catch $e
            end))
        "#,
    );
    assert_eq!(call_i32(&mut store, instance, "run", &[]), Ok(2));
}

#[test]
fn loops_reenter_try_and_catch_every_iteration() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (func (export "run") (param $n i32) (result i32)
            (local $sum i32)
            loop $again
              try
                local.get $n
                throw $e
              catch $e
                local.get $sum
                i32.add
                local.set $sum
              end
              local.get $n
              i32.const 1
              i32.sub
              local.tee $n
              br_if $again
            end
            local.get $sum))
        "#,
    );
    assert_eq!(call_i32(&mut store, instance, "run", &[Val::I32(100)]), Ok(5050));
}

#[test]
fn exceptions_unwind_frames_and_preserve_caller_operands() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $a (param i64 i32))
          (tag $b)
          (func $thrower (param i32)
            local.get 0
            if
              i64.const 40
              i32.const 2
              throw $a
            end
            throw $b)
          (func $middle (param i32) (result i32)
            i32.const 1000
            local.get 0
            call $thrower
            drop
            i32.const -1)
          (func (export "run") (param i32) (result i32)
            i32.const 7
            try (result i32)
              local.get 0
              call $middle
            catch $a
              i64.extend_i32_u
              i64.add
              i32.wrap_i64
            catch_all
              i32.const 99
            end
            i32.add))
        "#,
    );
    assert_eq!(call_i32(&mut store, instance, "run", &[Val::I32(1)]), Ok(49));
    assert_eq!(call_i32(&mut store, instance, "run", &[Val::I32(0)]), Ok(106));
}

#[test]
fn rethrow_selects_the_catch_label_and_uncaught_errors() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (tag $f (param i32))
          (func (export "outer") (param i32) (result i32)
            try (result i32)
              try (result i32)
                local.get 0
                throw $e
              catch $e
                drop
                try (result i32)
                  i32.const 5
                  throw $f
                catch $f
                  drop
                  rethrow 1
                end
              end
            catch $e
              i32.const 100
              i32.add
            end)
          (func (export "unmatched") (result i32)
            try (result i32)
              i32.const 1
              throw $f
            catch $e
            end))
        "#,
    );
    assert_eq!(call_i32(&mut store, instance, "outer", &[Val::I32(3)]), Ok(103));
    let error = call_i32(&mut store, instance, "unmatched", &[]).unwrap_err();
    assert!(error.contains("uncaught WebAssembly exception"), "{error}");
    // A following call starts without a stale pending or caught exception.
    assert_eq!(call_i32(&mut store, instance, "outer", &[Val::I32(1)]), Ok(101));
}

#[test]
fn catch_bodies_are_not_covered_by_their_own_handler() {
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e)
          (func (export "run") (result i32)
            try (result i32)
              try (result i32)
                throw $e
              catch $e
                throw $e
              end
            catch $e
              i32.const 9
            end))
        "#,
    );
    assert_eq!(call_i32(&mut store, instance, "run", &[]), Ok(9));
}

#[test]
fn delegate_targets_the_try_at_its_label() {
    // The legacy explainer's "Try-delegate blocks" example: `delegate l` counts the labels
    // outside its `try` and hands the exception to the innermost `try` body at or beyond that
    // label. A `block` label or a `try` whose catch clause is executing passes it outward,
    // and the function body label passes it to the caller.
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e)
          (func $throw throw $e)
          ;; $site selects the delegate; the result names the catching clause:
          ;; 1 = catch ($lC), 2 = catch ($lA), 3 = the caller.
          (func $test (param $site i32) (param $in_catch i32) (result i32)
            try $lA (result i32)
              block $lB (result i32)
                try $lC (result i32)
                  local.get $in_catch
                  if
                    call $throw
                  end
                  local.get $site
                  i32.const 0
                  i32.eq
                  if
                    try
                      call $throw
                    delegate $lC
                  end
                  local.get $site
                  i32.const 1
                  i32.eq
                  if
                    try
                      call $throw
                    delegate $lB
                  end
                  local.get $site
                  i32.const 2
                  i32.eq
                  if
                    try
                      call $throw
                    delegate $lA
                  end
                  try
                    call $throw
                  delegate 3
                  i32.const 0
                catch $e
                  local.get $in_catch
                  i32.eqz
                  if
                    i32.const 1
                    return
                  end
                  local.get $site
                  i32.const 0
                  i32.eq
                  if
                    try
                      call $throw
                    delegate $lC
                  end
                  local.get $site
                  i32.const 1
                  i32.eq
                  if
                    try
                      call $throw
                    delegate $lB
                  end
                  local.get $site
                  i32.const 2
                  i32.eq
                  if
                    try
                      call $throw
                    delegate $lA
                  end
                  try
                    call $throw
                  delegate 3
                  i32.const 0
                end
              end
            catch $e
              i32.const 2
            end)
          (func (export "run") (param i32 i32) (result i32)
            try (result i32)
              local.get 0
              local.get 1
              call $test
            catch $e
              i32.const 3
            end))
        "#,
    );
    // In the body of `try $lC`: the delegates to $lC, $lB, $lA and the function body.
    let body = [1, 2, 2, 3];
    // In the catch clause of `try $lC`, whose handler is no longer active.
    let in_catch = [2, 2, 2, 3];
    for (site, (body, in_catch)) in body.into_iter().zip(in_catch).enumerate() {
        let site = Val::I32(site as i32);
        assert_eq!(
            call_i32(&mut store, instance, "run", &[site.clone(), Val::I32(0)]),
            Ok(body),
            "body {site:?}"
        );
        assert_eq!(
            call_i32(&mut store, instance, "run", &[site.clone(), Val::I32(1)]),
            Ok(in_catch),
            "catch {site:?}"
        );
    }
}

#[test]
fn delegate_skips_handlers_of_enclosing_frames_only_in_its_own_frame() {
    // A delegate to the function body continues at the caller's handlers, while an
    // exception that is not delegated is still caught by the innermost handler.
    let (mut store, instance) = instantiate(
        r#"
        (module
          (tag $e (param i32))
          (func $inner (param i32)
            try
              try
                local.get 0
                throw $e
              delegate 1
            catch $e
              drop
            end)
          (func (export "run") (param i32) (result i32)
            try (result i32)
              local.get 0
              call $inner
              i32.const -1
            catch $e
            end))
        "#,
    );
    assert_eq!(call_i32(&mut store, instance, "run", &[Val::I32(7)]), Ok(7));
}

/// A node of a WebAssembly text s-expression.
enum Sexp {
    Atom(String),
    List(Vec<Sexp>),
}

/// Parses `src` into s-expressions, dropping comments.
fn parse_sexps(src: &str) -> Vec<Sexp> {
    fn parse_seq(chars: &[char], pos: &mut usize, nested: bool) -> Vec<Sexp> {
        let mut items = Vec::new();
        while *pos < chars.len() {
            let c = chars[*pos];
            if c.is_whitespace() {
                *pos += 1;
            } else if c == ';' && chars.get(*pos + 1) == Some(&';') {
                while *pos < chars.len() && chars[*pos] != '\n' {
                    *pos += 1;
                }
            } else if c == '(' && chars.get(*pos + 1) == Some(&';') {
                let mut depth = 0;
                while *pos < chars.len() {
                    if chars[*pos] == '(' && chars.get(*pos + 1) == Some(&';') {
                        depth += 1;
                        *pos += 2;
                    } else if chars[*pos] == ';' && chars.get(*pos + 1) == Some(&')') {
                        depth -= 1;
                        *pos += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        *pos += 1;
                    }
                }
            } else if c == '(' {
                *pos += 1;
                items.push(Sexp::List(parse_seq(chars, pos, true)));
            } else if c == ')' {
                assert!(nested, "unbalanced `)`");
                *pos += 1;
                return items;
            } else {
                let start = *pos;
                if c == '"' {
                    *pos += 1;
                    while chars[*pos] != '"' {
                        *pos += if chars[*pos] == '\\' { 2 } else { 1 };
                    }
                    *pos += 1;
                } else {
                    while *pos < chars.len()
                        && !chars[*pos].is_whitespace()
                        && chars[*pos] != '('
                        && chars[*pos] != ')'
                    {
                        *pos += 1;
                    }
                }
                items.push(Sexp::Atom(chars[start..*pos].iter().collect()));
            }
        }
        items
    }
    let chars: Vec<char> = src.chars().collect();
    parse_seq(&chars, &mut 0, false)
}

/// Rewrites the folded legacy `(try .. (do ..) (catch ..) (catch_all ..))` and
/// `(try .. (do ..) (delegate l))` forms, which current text parsers no longer accept, into
/// their equivalent flat instruction sequences.
fn unfold_legacy_try(items: Vec<Sexp>) -> Vec<Sexp> {
    let atom = |text: &str| Sexp::Atom(String::from(text));
    let head = |items: &[Sexp]| match items.first() {
        Some(Sexp::Atom(head)) => Some(head.clone()),
        _ => None,
    };
    let mut out = Vec::new();
    for item in items {
        let Sexp::List(list) = item else {
            out.push(item);
            continue;
        };
        let is_folded_try = head(&list).as_deref() == Some("try")
            && list
                .iter()
                .any(|item| matches!(item, Sexp::List(l) if head(l).as_deref() == Some("do")));
        if !is_folded_try {
            out.push(Sexp::List(unfold_legacy_try(list)));
            continue;
        }
        let mut delegated = false;
        for part in list {
            match part {
                Sexp::List(clause) => match head(&clause).as_deref() {
                    Some("do") => out.extend(unfold_legacy_try(clause.into_iter().skip(1).collect())),
                    Some("catch" | "catch_all" | "delegate") => {
                        delegated |= head(&clause).as_deref() == Some("delegate");
                        let mut clause = clause.into_iter();
                        let keyword = clause.next().unwrap();
                        out.push(keyword);
                        out.extend(unfold_legacy_try(clause.collect()));
                    }
                    _ => out.push(Sexp::List(clause)),
                },
                atom_part => out.push(atom_part),
            }
        }
        if !delegated {
            out.push(atom("end"));
        }
    }
    out
}

fn print_sexps(items: &[Sexp], out: &mut String) {
    for item in items {
        match item {
            Sexp::Atom(text) => out.push_str(text),
            Sexp::List(list) => {
                out.push('(');
                print_sexps(list, out);
                out.push(')');
            }
        }
        out.push(' ');
    }
}

fn run_script(path: &Path) -> (usize, Vec<String>) {
    // Tags cannot be imported: give `try_catch.wast`'s main module a local tag instead, and
    // skip only the assertion that depends on the identity of the imported tag.
    let original = std::fs::read_to_string(path)
        .unwrap()
        .replace(r#"(tag $imported-e0 (import "test" "e0"))"#, "(tag $imported-e0)");
    let mut source = String::new();
    print_sexps(&unfold_legacy_try(parse_sexps(&original)), &mut source);
    Runner::new(&["catch-imported"]).run_script(&path.display().to_string(), &source)
}

#[test]
#[ignore]
fn legacy_exception_spec() {
    let dir = std::env::var_os("WASMI_LEGACY_EH_DIR").expect("set WASMI_LEGACY_EH_DIR");
    for script in ["throw.wast", "try_catch.wast", "rethrow.wast", "try_delegate.wast"] {
        let (passed, skips) = run_script(&Path::new(&dir).join(script));
        std::println!("{script}: {passed} directives, {} skipped", skips.len());
        for skip in skips {
            std::println!("  skipped {skip}");
        }
    }
}
