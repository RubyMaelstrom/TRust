use crate::{
    JsString, SpannedSourceText,
    builtins::function::ThisMode,
    bytecompiler::ByteCompiler,
    js_string,
    vm::{CodeBlock, CodeBlockFlags, source_info::SourcePath},
};
use boa_ast::{
    function::{FormalParameterList, FunctionBody},
    scope::{FunctionScopes, Scope},
};
use boa_gc::Gc;
use boa_interner::Interner;
use boa_parser::{Parser, Source};

/// `FunctionCompiler` is used to compile AST functions to bytecode.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct FunctionCompiler {
    name: JsString,
    generator: bool,
    r#async: bool,
    strict: bool,
    arrow: bool,
    method: bool,
    in_with: bool,
    force_function_scope: bool,
    /// TRust lazy parsing: whether this function is compiled as Module code, so a
    /// deferred body re-parses in the Module goal (permits `import.meta`).
    in_module: bool,
    name_scope: Option<Scope>,
    spanned_source_text: SpannedSourceText,
    source_path: SourcePath,
}

impl FunctionCompiler {
    /// Create a new `FunctionCompiler`.
    pub(crate) fn new(spanned_source_text: SpannedSourceText) -> Self {
        Self {
            name: js_string!(),
            generator: false,
            r#async: false,
            strict: false,
            arrow: false,
            method: false,
            in_with: false,
            force_function_scope: false,
            in_module: false,
            name_scope: None,
            spanned_source_text,
            source_path: SourcePath::None,
        }
    }

    /// Set the name of the function.
    pub(crate) fn name<N>(mut self, name: N) -> Self
    where
        N: Into<Option<JsString>>,
    {
        let name = name.into();
        if let Some(name) = name {
            self.name = name;
        }
        self
    }

    /// Indicate if the function is an arrow function.
    pub(crate) const fn arrow(mut self, arrow: bool) -> Self {
        self.arrow = arrow;
        self
    }
    /// Indicate if the function is a method function.
    pub(crate) const fn method(mut self, method: bool) -> Self {
        self.method = method;
        self
    }
    /// Indicate if the function is a generator function.
    pub(crate) const fn generator(mut self, generator: bool) -> Self {
        self.generator = generator;
        self
    }

    /// Indicate if the function is an async function.
    pub(crate) const fn r#async(mut self, r#async: bool) -> Self {
        self.r#async = r#async;
        self
    }

    /// Indicate if the function is in a strict context.
    pub(crate) const fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Provide the name scope of the function.
    pub(crate) fn name_scope(mut self, name_scope: Option<Scope>) -> Self {
        self.name_scope = name_scope;
        self
    }

    /// Indicate if the function is in a `with` statement.
    pub(crate) const fn in_with(mut self, in_with: bool) -> Self {
        self.in_with = in_with;
        self
    }

    /// Indicate if the function is compiled as Module code (TRust lazy parsing:
    /// a deferred body re-parses in the Module goal, permitting `import.meta`).
    pub(crate) const fn in_module(mut self, in_module: bool) -> Self {
        self.in_module = in_module;
        self
    }

    /// Indicate if the function is in a `with` statement.
    pub(crate) const fn force_function_scope(mut self, force_function_scope: bool) -> Self {
        self.force_function_scope = force_function_scope;
        self
    }

    /// Set source map file path.
    pub(crate) fn source_path(mut self, source_path: SourcePath) -> Self {
        self.source_path = source_path;
        self
    }

    /// Compile a function statement list and it's parameters into bytecode.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn compile(
        mut self,
        parameters: &FormalParameterList,
        body: &FunctionBody,
        variable_environment: Scope,
        lexical_environment: Scope,
        scopes: &FunctionScopes,
        contains_direct_eval: bool,
        interner: &mut Interner,
    ) -> Gc<CodeBlock> {
        // A body skipped at parse time (TRust lazy *parsing*) has no statements
        // and can only be stubbed via `compile_or_lazy`; it must never reach the
        // eager `compile`. The eager-IIFE path guards on `!body.is_lazy()` and
        // the delazify re-parse forces its outermost body eager, so this holds.
        debug_assert!(
            !body.is_lazy(),
            "a lazily-skipped function body must be stubbed, never eagerly compiled"
        );

        self.strict = self.strict || body.strict();

        let length = parameters.length();

        let mut compiler = ByteCompiler::new(
            self.name,
            self.strict,
            false,
            variable_environment,
            lexical_environment,
            self.r#async,
            self.generator,
            interner,
            self.in_with,
            self.spanned_source_text,
            self.source_path,
        );

        // TRust lazy parsing: the child inherits the Module goal, so functions
        // deferred within this body also re-parse as Module code.
        compiler.in_module = self.in_module;
        compiler.length = length;
        compiler.code_block_flags.set(
            CodeBlockFlags::HAS_PROTOTYPE_PROPERTY,
            !self.arrow && !self.method && !self.r#async && !self.generator,
        );

        if self.arrow {
            compiler.this_mode = ThisMode::Lexical;
        }

        if let Some(scope) = self.name_scope
            && !scope.all_bindings_local()
        {
            compiler.code_block_flags |= CodeBlockFlags::HAS_BINDING_IDENTIFIER;
            let _ = compiler.push_scope(&scope);
        }

        if contains_direct_eval || !scopes.function_scope().all_bindings_local() {
            compiler.code_block_flags |= CodeBlockFlags::HAS_FUNCTION_SCOPE;
        } else if !self.arrow {
            compiler.code_block_flags.set(
                CodeBlockFlags::HAS_FUNCTION_SCOPE,
                self.force_function_scope || scopes.requires_function_scope(),
            );
        }

        if compiler.code_block_flags.has_function_scope() {
            let _ = compiler.push_scope(scopes.function_scope());
        } else {
            compiler.variable_scope = scopes.function_scope().clone();
            compiler.lexical_scope = scopes.function_scope().clone();
        }

        // Taken from:
        //  - 15.9.3 Runtime Semantics: EvaluateAsyncConciseBody: <https://tc39.es/ecma262/#sec-runtime-semantics-evaluateasyncconcisebody>
        //  - 15.8.4 Runtime Semantics: EvaluateAsyncFunctionBody: <https://tc39.es/ecma262/#sec-runtime-semantics-evaluateasyncfunctionbody>
        //
        // Note: In `EvaluateAsyncGeneratorBody` unlike the async non-generator functions we don't handle exceptions thrown by
        // `FunctionDeclarationInstantiation` (so they are propagated).
        //
        // See: 15.6.2 Runtime Semantics: EvaluateAsyncGeneratorBody: https://tc39.es/ecma262/#sec-runtime-semantics-evaluateasyncgeneratorbody
        if compiler.is_async() && !compiler.is_generator() {
            // 1. Let promiseCapability be ! NewPromiseCapability(%Promise%).
            //
            // Note: If the promise capability is already set, then we do nothing.
            // This is a deviation from the spec, but it allows to set the promise capability by
            // ExecuteAsyncModule ( module ): <https://tc39.es/ecma262/#sec-execute-async-module>
            compiler.bytecode.emit_create_promise_capability();

            // 2. Let declResult be Completion(FunctionDeclarationInstantiation(functionObject, argumentsList)).
            //
            // Note: We push an exception handler so we catch exceptions that are thrown by the
            // `FunctionDeclarationInstantiation` abstract function.
            //
            // Patched in `ByteCompiler::finish()`.
            compiler.async_handler = Some(compiler.push_handler());
        }

        compiler.function_declaration_instantiation(
            body,
            parameters,
            self.arrow,
            self.strict,
            self.generator,
            scopes,
        );

        // Taken from:
        // - 27.6.3.2 AsyncGeneratorStart ( generator, generatorBody ): <https://tc39.es/ecma262/#sec-asyncgeneratorstart>
        //
        // Note: We do handle exceptions thrown by generator body in `AsyncGeneratorStart`.
        if compiler.is_generator() {
            assert!(compiler.async_handler.is_none());

            if compiler.is_async() {
                // Patched in `ByteCompiler::finish()`.
                compiler.async_handler = Some(compiler.push_handler());
            }
        }

        {
            let mut compiler = compiler.position_guard(body);
            compiler.compile_statement_list(body.statement_list(), false, false);
        }

        compiler.params = parameters.clone();
        compiler.parameter_scope = scopes.parameter_scope();

        let code = compiler.finish();

        Gc::new(code)
    }

    /// Whether this function may be compiled lazily (TRust lazy compilation —
    /// see [`crate::vm::lazy`]): lazy is enabled and not suppressed on this
    /// thread, the function is an ordinary one (not a generator/async/arrow/
    /// method — those keep the exact eager path this first cut targets), not
    /// lexically inside a `with`, free of direct `eval`, and its source span is
    /// real and at least the size threshold (so the stub's source/`toString` is
    /// correct and the retained AST is worth its cost). Class constructors carry
    /// a binding-environment shape these inputs don't capture, so they are
    /// compiled through `class.rs`'s own path, never here.
    fn is_lazy_eligible(&self, contains_direct_eval: bool) -> bool {
        crate::vm::lazy::should_defer()
            && !self.generator
            && !self.r#async
            && !self.arrow
            && !self.method
            && !self.in_with
            && !contains_direct_eval
            && self
                .spanned_source_text
                .to_code_points()
                .is_some_and(|cps| cps.len() >= crate::vm::lazy::min_source_len())
    }

    /// Compile this function, deferring its body when [eligible](Self::is_lazy_eligible):
    /// the single seam every deferrable call site (function expressions, all
    /// function/generator/async **declaration** instantiations) routes through
    /// to get lazy compilation. An ineligible function (or lazy disabled) takes
    /// the exact eager [`compile`](Self::compile) path. The delazify path
    /// (`LazyFunctionData::compile`) calls `compile` directly, so it never
    /// re-defers.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn compile_or_lazy(
        self,
        parameters: &FormalParameterList,
        body: &FunctionBody,
        variable_environment: Scope,
        lexical_environment: Scope,
        scopes: &FunctionScopes,
        contains_direct_eval: bool,
        interner: &mut Interner,
    ) -> Gc<CodeBlock> {
        // A body skipped at parse time (TRust lazy *parsing*) carries no
        // statements, so it MUST be stubbed regardless of the compile-time
        // eligibility heuristics — its real body is recovered by re-parsing the
        // retained span on first call. An ordinarily-parsed body defers only
        // when `is_lazy_eligible` (TRust lazy *compilation*).
        if body.is_lazy() || self.is_lazy_eligible(contains_direct_eval) {
            self.compile_lazy_stub(parameters, body, variable_environment, lexical_environment)
        } else {
            self.compile(
                parameters,
                body,
                variable_environment,
                lexical_environment,
                scopes,
                contains_direct_eval,
                interner,
            )
        }
    }

    /// Build a **lazy stub** [`CodeBlock`] for this function instead of compiling
    /// its body (TRust lazy compilation — see [`crate::vm::lazy`]).
    ///
    /// The stub carries every observable function attribute that is available
    /// without compiling the body — `name`, `length`, strictness, the
    /// prototype-property and this-mode — so the function *object* created from
    /// it (`create_function_object_fast`) and `Function.prototype.toString`
    /// (which reads the retained source span) are correct *before the first
    /// call*. The body's compile inputs are retained in [`LazyFunctionData`]; the
    /// call funnels compile the real block on first invocation
    /// ([`LazyFunctionData::compile`]).
    ///
    /// The caller (`ByteCompiler::function`) must only defer functions for which
    /// these cheap attributes fully determine pre-call observable behaviour —
    /// ordinary functions (no generator/async/arrow/method/direct-`eval`), with a
    /// real source span.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn compile_lazy_stub(
        self,
        parameters: &FormalParameterList,
        body: &FunctionBody,
        variable_environment: Scope,
        lexical_environment: Scope,
    ) -> Gc<CodeBlock> {
        // Mirror the early, body-independent part of `compile`: `length` and the
        // effective strictness (`compile` ORs in `body.strict()`), so the stub's
        // `.length` own-property and `STRICT` flag match the real block exactly.
        let length = parameters.length();
        let strict = self.strict || body.strict();
        let has_prototype_property =
            !self.arrow && !self.method && !self.r#async && !self.generator;
        let this_mode = if self.arrow {
            ThisMode::Lexical
        } else {
            ThisMode::Global
        };

        // The stub's `source_info` needs the function's name, path and source
        // span (for `.name` and `Function.prototype.toString` before any call);
        // clone these cheap handles before the rest moves into `LazyFunctionData`.
        let name = self.name.clone();
        let source_path = self.source_path.clone();
        let spanned_source_text = self.spanned_source_text.clone();

        // Phase C retains NO body AST — only the span and the few facts needed to
        // re-parse + re-analyze on first call. `parameters`/`body`/`scopes`/
        // `contains_direct_eval` are recovered by the re-parse, not stored.
        //
        // A named function *expression* has a self-name scope; the collector
        // creates that scope iff the node's `has_binding_identifier` is set, so
        // `name_scope.is_some()` recovers the original flag (a declaration's name
        // lives in the enclosing scope, leaving `name_scope` `None`). `scopes` and
        // `contains_direct_eval` are recovered by the re-parse, not retained.
        let lazy = LazyFunctionData {
            name: self.name,
            has_binding_identifier: self.name_scope.is_some(),
            // Store the ORIGINAL strictness; delazify re-derives `|| body.strict()`
            // exactly as `compile` does, so the recompile is identical.
            strict: self.strict,
            // Store the goal symbol so the first-call re-parse matches it.
            module: self.in_module,
            spanned_source_text: self.spanned_source_text,
            source_path: self.source_path,
            variable_environment,
            lexical_environment,
        };

        Gc::new(CodeBlock::new_lazy_stub(
            name,
            length,
            strict,
            has_prototype_property,
            this_mode,
            source_path,
            spanned_source_text,
            Box::new(lazy),
        ))
    }
}

/// The retained inputs of a function whose bytecode generation was deferred
/// (TRust lazy compilation — see [`crate::vm::lazy`]). Held by a lazy stub
/// [`CodeBlock`] and consumed by [`LazyFunctionData::compile`] on first call to
/// produce the real block.
///
/// **Phase C (span + re-parse):** unlike the original clone-retain prototype,
/// this does NOT keep the body AST. It keeps only the function's source span and
/// the small set of facts needed to re-derive everything on first call: the
/// name, whether the function had a self-referential binding (declaration vs.
/// named expression), the enclosing strictness, the source path, and the
/// enclosing variable/lexical environments. On first call,
/// [`compile`](Self::compile) RE-PARSES the function from its span and
/// RE-ANALYZES it against the retained enclosing scope, then compiles — dropping
/// the retained AST that made the clone-retain prototype regress peak RSS.
///
/// Only ordinary functions are deferred (no generator/async/arrow/method —
/// [`FunctionCompiler::is_lazy_eligible`]), so those flags are implicitly
/// `false` here and need not be stored.
///
/// It holds no `Gc` pointers (`Scope` is `Rc`-backed, `JsString`/`SourcePath`/
/// the source span are owned or reference-counted, none GC-managed), so the
/// field on [`CodeBlock`] is soundly `#[unsafe_ignore_trace]`, like
/// `Constant::Scope`.
#[derive(Clone, Debug)]
pub(crate) struct LazyFunctionData {
    /// The function's `.name` (used for the compiled block; the re-parsed
    /// expression's own name handling is overridden by `has_binding_identifier`).
    name: JsString,
    /// Whether the original function had a self-referential binding identifier (a
    /// named function *expression*). A function *declaration* binds its name in
    /// the enclosing scope, so it must be re-parsed with this `false` or the
    /// re-analysis would create a spurious self-name scope. Equal to the original
    /// `name_scope.is_some()` (the collector creates that scope iff
    /// `has_binding_identifier`).
    has_binding_identifier: bool,
    /// The enclosing strictness (before `|| body.strict()`); the re-parse and
    /// re-analysis use it, and `compile` re-derives the effective strictness from
    /// the body exactly as the eager path does.
    strict: bool,
    /// Whether the function was compiled as Module code. The first-call re-parse
    /// must use the same goal symbol as the original parse: Module code permits
    /// `import.meta` (which a deferred Vite dynamic-import helper references),
    /// while Script code does not. Without it such a body re-parses in the Script
    /// goal and is rejected as "invalid `import.meta` expression outside a
    /// module", surfacing as the throwing fallback ("deferred function body
    /// failed to parse").
    module: bool,
    /// The function's source span — re-parsed on first call to recover the body
    /// AST without having retained it.
    spanned_source_text: SpannedSourceText,
    source_path: SourcePath,
    /// The enclosing environments captured at deferral. The re-analysis links the
    /// function's fresh scopes to `lexical_environment` (the scope the eager
    /// analysis used as the function's parent); `compile` receives both, exactly
    /// as the eager `ByteCompiler::function` passed them.
    variable_environment: Scope,
    lexical_environment: Scope,
}

impl LazyFunctionData {
    /// Compile the deferred function into its real [`CodeBlock`] on first call by
    /// re-parsing and re-analyzing it (TRust lazy compilation Phase C).
    ///
    /// Steps, mirroring the eager path so the output is behaviourally identical:
    /// 1. Re-parse the retained source span as a function expression. The span
    ///    was already validated by the eager parse, so this cannot fail.
    /// 2. Force the original `has_binding_identifier` so a re-parsed *declaration*
    ///    does not gain a self-name scope.
    /// 3. Re-analyze against the retained enclosing scope. This rebuilds the
    ///    function's own scopes AND its nested functions' scopes (which the eager
    ///    analysis embedded in the now-dropped AST). The enclosing scope's escape
    ///    flags were already set by the eager analysis, so the re-marking
    ///    `analyze_scope` performs on it is idempotent.
    /// 4. Compile with the retained enclosing environments and the freshly
    ///    analyzed function scopes.
    ///
    /// `interner` is the page `Context`'s persistent interner, so re-parsed
    /// identifiers resolve to the same names the retained scopes were keyed by
    /// (scopes key bindings by `JsString`). `parser_identifier` makes any
    /// tagged-template call sites in the body unique within the page.
    pub(crate) fn compile(&self, interner: &mut Interner, parser_identifier: u32) -> Gc<CodeBlock> {
        self.try_compile(interner, parser_identifier)
            .unwrap_or_else(|| self.compile_throwing_fallback(interner, parser_identifier))
    }

    /// Re-parse, re-analyze, and compile the deferred function. Returns `None`
    /// if the re-parse or re-analysis fails.
    ///
    /// With lazy *compilation* (C1) the body was eager-parsed before being
    /// dropped, so this never fails. With lazy *parsing* (C2) the body was only
    /// brace-scanned, not validated, so a body the scanner accepted but that is
    /// not in fact valid JS (which the eager parser would have rejected at load,
    /// and which never occurs in a page that loaded) fails here; the caller then
    /// compiles a throwing fallback so the error surfaces at call time rather
    /// than aborting.
    fn try_compile(&self, interner: &mut Interner, parser_identifier: u32) -> Option<Gc<CodeBlock>> {
        let code_points = self
            .spanned_source_text
            .to_code_points()
            .expect("a deferred function always retains a real source span");

        let mut parser = Parser::new(Source::from_utf16(code_points));
        if self.strict {
            parser.set_strict();
        }
        // The re-parse must use the SAME goal symbol as the original parse: a
        // body deferred from Module code may reference `import.meta`, which the
        // default Script goal rejects (Vite's dynamic-import helper does this).
        if self.module {
            parser.set_module();
        }
        parser.set_identifier(parser_identifier);
        let (mut function, reparsed_source) = parser.parse_function_expression(interner).ok()?;

        function.set_has_binding_identifier(self.has_binding_identifier);

        function
            .analyze_scope(self.strict, &self.lexical_environment, interner)
            .ok()?;

        // The re-parse's AST spans are relative to the EXTRACTED function source,
        // so the compiled block (and any nested function it defers in turn) must
        // carry that extracted text — spanning all of it — as their source base.
        // Using the retained original-document span here would slice nested
        // functions' source out of the wrong coordinate system.
        let spanned_source_text = SpannedSourceText::from_full_source(reparsed_source);

        let block = FunctionCompiler {
            name: self.name.clone(),
            generator: false,
            r#async: false,
            strict: self.strict,
            arrow: false,
            method: false,
            in_with: false,
            force_function_scope: false,
            // Nested functions deferred within this delazified body inherit the
            // Module goal, so they too re-parse permitting `import.meta`.
            in_module: self.module,
            name_scope: function.name_scope().cloned(),
            spanned_source_text,
            source_path: self.source_path.clone(),
        }
        .compile(
            function.parameters(),
            function.body(),
            self.variable_environment.clone(),
            self.lexical_environment.clone(),
            function.scopes(),
            function.contains_direct_eval(),
            interner,
        );
        Some(block)
    }

    /// Compile a stand-in whose body throws a `SyntaxError` on call, for the rare
    /// case ([`try_compile`](Self::try_compile)) of a lazily-*parsed* body the
    /// scanner accepted but the parser rejects. Keeps the function's name; its
    /// synthetic source is always valid, so this cannot itself fail.
    fn compile_throwing_fallback(
        &self,
        interner: &mut Interner,
        parser_identifier: u32,
    ) -> Gc<CodeBlock> {
        const FALLBACK: &str =
            "function(){throw new SyntaxError('deferred function body failed to parse')}";
        let code_points: Vec<u16> = FALLBACK.encode_utf16().collect();
        let mut parser = Parser::new(Source::from_utf16(&code_points));
        parser.set_identifier(parser_identifier);
        let (mut function, reparsed_source) = parser
            .parse_function_expression(interner)
            .expect("the synthetic fallback source is always valid");
        function.set_has_binding_identifier(false);
        function
            .analyze_scope(false, &self.lexical_environment, interner)
            .expect("the synthetic fallback source always analyzes");
        let spanned_source_text = SpannedSourceText::from_full_source(reparsed_source);
        FunctionCompiler {
            name: self.name.clone(),
            generator: false,
            r#async: false,
            strict: false,
            arrow: false,
            method: false,
            in_with: false,
            force_function_scope: false,
            // The synthetic fallback body holds no nested functions; goal is moot.
            in_module: self.module,
            name_scope: function.name_scope().cloned(),
            spanned_source_text,
            source_path: self.source_path.clone(),
        }
        .compile(
            function.parameters(),
            function.body(),
            self.variable_environment.clone(),
            self.lexical_environment.clone(),
            function.scopes(),
            function.contains_direct_eval(),
            interner,
        )
    }
}
