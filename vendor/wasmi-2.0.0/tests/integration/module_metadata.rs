use wasmi::{Engine, Module};

#[test]
fn imports_and_exports_preserve_declaration_order() {
    // WebAssembly JS API consumers expose these iterators directly, so their
    // order must follow the module's import and export sections rather than
    // registry grouping or hash-map iteration.
    let wasm = wat::parse_str(
        r#"
        (module
          (import "env" "f" (func $f))
          (import "env" "g" (global $g i32))
          (import "env" "m" (memory $m 1))
          (import "env" "t" (table $t 1 externref))
          (export "m" (memory $m))
          (export "t" (table $t))
          (export "g" (global $g))
          (export "f" (func $f)))
        "#,
    )
    .unwrap();
    let module = Module::new(&Engine::default(), &wasm).unwrap();

    let imports: Vec<_> = module
        .imports()
        .map(|import| (import.module(), import.name()))
        .collect();
    assert_eq!(
        imports,
        [("env", "f"), ("env", "g"), ("env", "m"), ("env", "t")]
    );
    let exports: Vec<_> = module.exports().map(|export| export.name()).collect();
    assert_eq!(exports, ["m", "t", "g", "f"]);
}
