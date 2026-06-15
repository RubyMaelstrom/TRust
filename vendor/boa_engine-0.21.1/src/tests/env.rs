use boa_macros::js_str;
use indoc::indoc;

use crate::{JsNativeErrorKind, TestAction, run_test_actions};

#[test]
// https://github.com/boa-dev/boa/issues/2317
fn fun_block_eval_2317() {
    run_test_actions([
        TestAction::assert_eq(
            indoc! {r#"
                (function(y){
                    {
                        eval("var x = 'inner';");
                    }
                    return y + x;
                })("arg");
            "#},
            js_str!("arginner"),
        ),
        TestAction::assert_eq(
            indoc! {r#"
                (function(y = "default"){
                    {
                        eval("var x = 'inner';");
                    }
                    return y + x;
                })();
            "#},
            js_str!("defaultinner"),
        ),
    ]);
}

#[test]
// https://github.com/boa-dev/boa/issues/2719
fn with_env_not_panic() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            with({ p1:1,  }) {k[oa>>2]=d;}
            {
            let a12345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890 = 1,
                b = "";
            }
        "#},
        JsNativeErrorKind::Reference,
        "k is not defined",
    )]);
}

#[test]
fn vue_template_render_function_with_body() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            var makeRender = new Function(
                'Vue',
                "const _Vue = Vue\n\n" +
                "return function render(_ctx, _cache) {\n" +
                "  with (_ctx) {\n" +
                "    const { toDisplayString: _toDisplayString } = _Vue;\n" +
                "    return _toDisplayString(who);\n" +
                "  }\n" +
                "}"
            );
            var render = makeRender({ toDisplayString: String });
            render({ who: 'TRust template' }, []);
        "#},
        js_str!("TRust template"),
    )]);
}

#[test]
fn mapped_arguments_inside_with_environment() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            (function(a) {
                with ({ shadow: true }) {
                    return arguments[0];
                }
            })(42);
        "#},
        42,
    )]);

    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            var render = new Function('with(this){ return arguments[0]; }');
            render.call({ shadow: true }, 44);
        "#},
        44,
    )]);
}

/// A closure defined inside a `with` body, that captures a block-scoped
/// binding declared in that body and also takes a parameter, must resolve
/// the captured binding to the correct environment. This is exactly the
/// shape Vue's full template compiler emits for `v-for`
/// (`_renderList(items, (it) => _toDisplayString(it.name))` inside
/// `with(_ctx)` of a `new Function`-built render).
///
/// FIXED (2026-06-15, sister) in `boa_ast`. `optimize_scope_indicies`
/// (`scope_analyzer.rs` `ScopeIndexVisitor`) re-indexes scopes so their
/// indices match the environments the VM actually pushes, collapsing
/// elided/all-local scopes. `Script`/`Module::analyze_scope` ran it but
/// `FunctionExpression::analyze_scope` (the `new Function` path) did not, so a
/// dynamically-compiled function kept `Scope::new`'s naive nesting indices;
/// a `with` inside it left every nested binding's locator one environment too
/// high, and a `v-for` callback pushing its own env on top landed the stale
/// index in the wrong env. The fix runs the pass for dynamic functions via
/// `optimize_function_scope_indicies`, which forces the root function's scope
/// slot to match the engine's `force_function_scope` (a naive invocation would
/// instead elide that slot and OOB-define the function's own captured `const`).
/// This subsumed the trap-#2 runtime clamp / `declarative_ref_at_or_below`
/// band-aids, which were removed.
#[test]
fn closure_in_with_captures_block_binding_through_parameter() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            var lib = {
                renderList: function (arr, cb) { return arr.map(cb).join(","); },
                display: function (x) { return String(x); },
            };
            var make = new Function(
                'lib',
                "const _lib = lib\n" +
                "return function render(ctx) {\n" +
                "  with (ctx) {\n" +
                "    const { renderList: _rl, display: _ds } = _lib;\n" +
                "    return _rl(items, function (it) { return _ds(it.name); });\n" +
                "  }\n" +
                "}"
            );
            var render = make(lib);
            render({ items: [{ name: 'a' }, { name: 'b' }] });
        "#},
        js_str!("a,b"),
    )]);

    // Arrow form of the same capture, plus a free variable resolved
    // through the `with` object environment inside the callback.
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            var lib = { display: function (x) { return String(x); } };
            var make = new Function(
                'lib',
                "const _lib = lib\n" +
                "return function render(ctx) {\n" +
                "  with (ctx) {\n" +
                "    const { display: _ds } = _lib;\n" +
                "    return items.map((it) => _ds(it) + ':' + prefix).join(',');\n" +
                "  }\n" +
                "}"
            );
            var render = make(lib);
            render({ items: [1, 2], prefix: 'p' });
        "#},
        js_str!("1:p,2:p"),
    )]);
}
