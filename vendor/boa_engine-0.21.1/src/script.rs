//! Boa's implementation of ECMAScript's Scripts.
//!
//! This module contains the [`Script`] type, which represents a [**Script Record**][script].
//!
//! More information:
//!  - [ECMAScript reference][spec]
//!
//! [spec]: https://tc39.es/ecma262/#sec-scripts
//! [script]: https://tc39.es/ecma262/#sec-script-records

use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use boa_ast::SourceText as AstSourceText;
use boa_gc::{Finalize, Gc, GcRefCell, Trace};
use boa_interner::Interner;
use boa_parser::{Parser, Source, source::ReadChar};

use crate::{
    Context, HostDefined, JsNativeError, JsResult, JsString, JsValue, Module, SpannedSourceText,
    bytecompiler::{ByteCompiler, global_declaration_instantiation_context},
    js_string,
    realm::Realm,
    spanned_source_text::SourceText,
    vm::{ActiveRunnable, CallFrame, CallFrameFlags, CodeBlock},
};

/// ECMAScript's [**Script Record**][spec].
///
/// [spec]: https://tc39.es/ecma262/#sec-script-records
#[derive(Clone, Trace, Finalize)]
pub struct Script {
    inner: Gc<Inner>,
}

impl std::fmt::Debug for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Script")
            .field("realm", &self.inner.realm.addr())
            .field("code", &self.inner.source)
            .field("loaded_modules", &self.inner.loaded_modules)
            .finish()
    }
}

#[derive(Trace, Finalize)]
struct Inner {
    realm: Realm,
    #[unsafe_ignore_trace]
    source: boa_ast::Script,
    source_text: SourceText,
    codeblock: GcRefCell<Option<Gc<CodeBlock>>>,
    loaded_modules: GcRefCell<FxHashMap<JsString, Module>>,
    host_defined: HostDefined,
    path: Option<PathBuf>,
}

/// A raw-parsed script paired with the private interner that named its
/// identifiers — the unit of work for parallel parsing (the JS-engine
/// performance plan's Step 5a).
///
/// [`Script::raw_parse`] produces one on *any* thread, with no [`Context`] and
/// without scope analysis (the bulk of the "parse" phase — lexing and AST
/// construction — thus runs off the page thread). [`Script::compile_raw`]
/// consumes it on the page thread, running the deferred scope analysis against
/// the *shared* realm scope and then compiling. Scope analysis and compilation
/// stay sequential there because global lexical bindings (`let`/`const`/
/// `class`) resolve at runtime by slot index into one shared global
/// environment, so their declaration order must be preserved.
#[derive(Debug)]
pub struct RawScript {
    code: boa_ast::Script,
    source: AstSourceText,
    interner: Interner,
    path: Option<PathBuf>,
}

// SAFETY: a `RawScript` is only ever *moved* between threads (a parse worker to
// the page thread), never shared (`&RawScript` does not cross threads). Its one
// field the compiler can't prove `Send` is `Interner`: its `InternedStr` map
// keys hold `NonNull` pointers into the interner's own `FixedString` heap
// buffers. Moving the interner moves the owners of those buffers but not the
// heap allocations themselves, so the pointers stay valid — a move is sound.
// Everything else is plainly movable: the raw (pre-`analyze_scope`) AST holds
// only `Sym`s (interner indices) and owned data — no `Rc`, `JsString`, or
// thread-affine handle — and `SourceText` is an owned UTF-16 buffer. There is
// no interior thread-affinity, so the whole value is sound to send.
unsafe impl Send for RawScript {}

impl Script {
    /// Gets the realm of this script.
    #[must_use]
    pub fn realm(&self) -> &Realm {
        &self.inner.realm
    }

    /// Returns the [`ECMAScript specification`][spec] defined [`\[\[HostDefined\]\]`][`HostDefined`] field of the [`Module`].
    ///
    /// [spec]: https://tc39.es/ecma262/#script-record
    #[must_use]
    pub fn host_defined(&self) -> &HostDefined {
        &self.inner.host_defined
    }

    /// Gets the loaded modules of this script.
    pub(crate) fn loaded_modules(&self) -> &GcRefCell<FxHashMap<JsString, Module>> {
        &self.inner.loaded_modules
    }

    /// Abstract operation [`ParseScript ( sourceText, realm, hostDefined )`][spec].
    ///
    /// Parses the provided `src` as an ECMAScript script, returning an error if parsing fails.
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-parse-script
    pub fn parse<R: ReadChar>(
        src: Source<'_, R>,
        realm: Option<Realm>,
        context: &mut Context,
    ) -> JsResult<Self> {
        let path = src.path().map(Path::to_path_buf);
        let mut parser = Parser::new(src);
        parser.set_identifier(context.next_parser_identifier());
        if context.is_strict() {
            parser.set_strict();
        }
        let scope = context.realm().scope().clone();
        let (mut code, source) = parser.parse_script_with_source(&scope, context.interner_mut())?;
        if !context.optimizer_options().is_empty() {
            context.optimize_statement_list(code.statements_mut());
        }

        let source_text = SourceText::new(source);

        Ok(Self {
            inner: Gc::new(Inner {
                realm: realm.unwrap_or_else(|| context.realm().clone()),
                source: code,
                source_text,
                codeblock: GcRefCell::default(),
                loaded_modules: GcRefCell::default(),
                host_defined: HostDefined::default(),
                path,
            }),
        })
    }

    /// Raw-parse `src` into a [`RawScript`] on the current thread — the worker
    /// half of parallel parse. Needs no [`Context`] and runs no scope analysis,
    /// so it can execute on any thread with a private interner.
    ///
    /// `identifier` must be a process-unique parser id (allocate one per script
    /// from [`Context::next_parser_identifier`] on the owning thread): it makes
    /// tagged-template call-site identities distinct across separately parsed
    /// scripts, exactly as the in-`Context` parse path does. The script is
    /// parsed in sloppy mode — a `"use strict"` prologue is honoured by the
    /// parser itself — matching [`Script::parse`], which only forces strict
    /// from [`Context::is_strict`] (always false for a top-level page script).
    ///
    /// Compile the result with [`Script::compile_raw`] on the page thread.
    ///
    /// # Errors
    /// Returns the formatted syntax error (a `String`, so it can cross back to
    /// the page thread) on any parse failure.
    pub fn raw_parse<R: ReadChar>(
        src: Source<'_, R>,
        identifier: u32,
    ) -> Result<RawScript, String> {
        let path = src.path().map(Path::to_path_buf);
        let mut interner = Interner::new();
        let mut parser = Parser::new(src);
        parser.set_identifier(identifier);
        let (code, source) = parser
            .parse_script_raw(&mut interner)
            .map_err(|e| e.to_string())?;
        Ok(RawScript {
            code,
            source,
            interner,
            path,
        })
    }

    /// Compile a [`RawScript`] (from [`Script::raw_parse`], possibly produced on
    /// another thread) into a runnable `Script` bound to `context`'s realm —
    /// the page-thread half of parallel parse.
    ///
    /// Runs the scope analysis `raw_parse` deferred, against the *shared* realm
    /// scope and in call order, so global lexical bindings get their slot
    /// indices in document order; then compiles the code block. The raw script's
    /// private interner is swapped into `context` for the duration of analysis +
    /// compilation (the AST names identifiers by that interner's `Sym`s) and the
    /// page interner is restored before returning — so anything the returned
    /// script does at runtime (e.g. a later `eval`) parses against the page
    /// interner as usual. This is sound because the resulting code block is
    /// interner-independent: it names every identifier by `JsString`, not by
    /// interner index, so the worker interner can be dropped once compilation
    /// finishes.
    ///
    /// # Errors
    /// Propagates scope-analysis and global-declaration syntax errors.
    pub fn compile_raw(
        raw: RawScript,
        realm: Option<Realm>,
        context: &mut Context,
    ) -> JsResult<Self> {
        let RawScript {
            code,
            source,
            mut interner,
            path,
        } = raw;
        // Swap the worker interner in so `analyze_scope` and the byte compiler
        // resolve the AST's `Sym` indices against the interner that produced
        // them. The page interner MUST be restored on every path — including a
        // panic out of the compiler (a Boa bug) — or the context would keep
        // wearing the worker interner; hence the catch/restore/resume below
        // rather than a plain swap pair.
        std::mem::swap(context.interner_mut(), &mut interner);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Self::compile_swapped(code, source, realm, path, context)
        }));
        std::mem::swap(context.interner_mut(), &mut interner);
        match outcome {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// The body of [`Script::compile_raw`], with the worker interner already
    /// installed as `context`'s interner. Mirrors [`Script::parse`]'s
    /// post-parse steps (analyze → optimize → wrap) and then compiles.
    fn compile_swapped(
        mut code: boa_ast::Script,
        source: AstSourceText,
        realm: Option<Realm>,
        path: Option<PathBuf>,
        context: &mut Context,
    ) -> JsResult<Self> {
        let scope = context.realm().scope().clone();
        code.analyze_scope(&scope, context.interner()).map_err(|reason| {
            JsNativeError::syntax().with_message(format!("invalid scope analysis: {reason}"))
        })?;
        if !context.optimizer_options().is_empty() {
            context.optimize_statement_list(code.statements_mut());
        }
        let source_text = SourceText::new(source);
        let script = Self {
            inner: Gc::new(Inner {
                realm: realm.unwrap_or_else(|| context.realm().clone()),
                source: code,
                source_text,
                codeblock: GcRefCell::default(),
                loaded_modules: GcRefCell::default(),
                host_defined: HostDefined::default(),
                path,
            }),
        };
        // Compile now, against the (swapped-in) worker interner — the produced
        // code block is interner-independent, so it stays valid once we restore
        // the page interner and drop the worker one.
        script.codeblock(context)?;
        Ok(script)
    }

    /// Compiles the codeblock of this script.
    ///
    /// This is a no-op if this has been called previously.
    pub fn codeblock(&self, context: &mut Context) -> JsResult<Gc<CodeBlock>> {
        let mut codeblock = self.inner.codeblock.borrow_mut();

        if let Some(codeblock) = &*codeblock {
            return Ok(codeblock.clone());
        }

        let mut annex_b_function_names = Vec::new();

        global_declaration_instantiation_context(
            &mut annex_b_function_names,
            &self.inner.source,
            self.inner.realm.scope(),
            context,
        )?;

        let spanned_source_text = SpannedSourceText::new_source_only(self.get_source());
        let mut compiler = ByteCompiler::new(
            js_string!("<main>"),
            self.inner.source.strict(),
            false,
            self.inner.realm.scope().clone(),
            self.inner.realm.scope().clone(),
            false,
            false,
            context.interner_mut(),
            false,
            spanned_source_text,
            self.path().map(Path::to_owned).into(),
        );

        #[cfg(feature = "annex-b")]
        {
            compiler.annex_b_function_names = annex_b_function_names;
        }

        // TODO: move to `Script::evaluate` to make this operation infallible.
        compiler.global_declaration_instantiation(&self.inner.source);
        compiler.compile_statement_list(self.inner.source.statements(), true, false);

        let cb = Gc::new(compiler.finish());

        *codeblock = Some(cb.clone());

        Ok(cb)
    }

    /// Whether re-running this script's compiled `<main>` block in a fresh realm
    /// reproduces **all** of its global-scope effects — i.e. its
    /// `GlobalDeclarationInstantiation` is fully *replayable from bytecode*, with
    /// no compile-time-only or shared-scope side effect that an
    /// install-the-block-and-evaluate path (the CDN compile cache) would skip.
    ///
    /// True iff the script declares:
    /// - **no global lexical bindings** (`let`/`const`/`class`): each would add a
    ///   binding by SLOT INDEX to the realm's shared, accumulating global
    ///   declarative scope *during scope analysis* — an index that depends on
    ///   what earlier scripts declared, and a mutation the cache path skips; and
    /// - **no Annex-B block-level function declarations**: those create a global
    ///   var binding at *compile* time (`GlobalDeclarationInstantiation`'s B.3.2
    ///   step), another effect the cache path would skip.
    ///
    /// Top-level `var` and `function` declarations ARE replayable: they create
    /// global-object bindings *by name* via `CreateGlobalVar/FunctionBinding`
    /// opcodes emitted into `<main>`, so simply running the block recreates them
    /// — and references to them resolve by name (`GlobalObject`), not by slot.
    ///
    /// This is the primary half of the cross-page CDN compile cache's
    /// realm-portability gate (JS-engine performance plan, Phase 2); the
    /// defensive half is
    /// [`CodeBlock::is_realm_portable`](crate::vm::CodeBlock::is_realm_portable),
    /// which additionally rejects a block that *reads* a global declarative slot
    /// an earlier script created.
    #[must_use]
    pub fn global_declarations_are_replayable(&self) -> bool {
        use boa_ast::operations::{annex_b_function_declarations_names, lexically_declared_names};
        let source = &self.inner.source;
        lexically_declared_names(source).is_empty()
            && annex_b_function_declarations_names(source).is_empty()
    }

    /// Installs a precompiled code block as this script's body, so the next
    /// evaluation runs it instead of compiling the source.
    ///
    /// This is the execution seam for a rehydrated
    /// [`CodeBlockImage`](crate::vm::CodeBlockImage)
    /// ([`CodeBlock::from_image`](crate::vm::CodeBlock::from_image)): parse a
    /// `Script` to obtain a realm, install a code block compiled elsewhere (an
    /// in-memory cache, a compile worker), then [`evaluate`](Self::evaluate). It
    /// replaces any previously compiled or installed block; the caller is
    /// responsible for the block having been compiled against an equivalent
    /// source and realm scope.
    pub fn set_codeblock(&self, codeblock: Gc<CodeBlock>) {
        *self.inner.codeblock.borrow_mut() = Some(codeblock);
    }

    /// Evaluates this script and returns its result.
    ///
    /// Note that this won't run any scheduled promise jobs; you need to call [`Context::run_jobs`]
    /// on the context or [`JobExecutor::run_jobs`] on the provided queue to run them.
    ///
    /// [`JobExecutor::run_jobs`]: crate::job::JobExecutor::run_jobs
    pub fn evaluate(&self, context: &mut Context) -> JsResult<JsValue> {
        self.prepare_run(context)?;
        let record = context.run();

        context.vm.pop_frame();
        record.consume()
    }

    /// Evaluates this script and returns its result, periodically yielding to the executor
    /// in order to avoid blocking the current thread.
    ///
    /// This uses an implementation defined amount of "clock cycles" that need to pass before
    /// execution is suspended. See [`Script::evaluate_async_with_budget`] if you want to also
    /// customize this parameter.
    #[allow(clippy::future_not_send)]
    pub async fn evaluate_async(&self, context: &mut Context) -> JsResult<JsValue> {
        self.evaluate_async_with_budget(context, 256).await
    }

    /// Evaluates this script and returns its result, yielding to the executor each time `budget`
    /// number of "clock cycles" pass.
    ///
    /// Note that "clock cycle" is in quotation marks because we can't determine exactly how many
    /// CPU clock cycles a VM instruction will take, but all instructions have a "cost" associated
    /// with them that depends on their individual complexity. We'd recommend benchmarking with
    /// different budget sizes in order to find the ideal yielding time for your application.
    #[allow(clippy::future_not_send)]
    pub async fn evaluate_async_with_budget(
        &self,
        context: &mut Context,
        budget: u32,
    ) -> JsResult<JsValue> {
        self.prepare_run(context)?;

        let record = context.run_async_with_budget(budget).await;

        context.vm.pop_frame();
        record.consume()
    }

    fn prepare_run(&self, context: &mut Context) -> JsResult<()> {
        let codeblock = self.codeblock(context)?;

        let env_fp = context.vm.environments.len() as u32;
        context.vm.push_frame_with_stack(
            CallFrame::new(
                codeblock,
                Some(ActiveRunnable::Script(self.clone())),
                context.vm.environments.clone(),
                self.inner.realm.clone(),
            )
            .with_env_fp(env_fp)
            .with_flags(CallFrameFlags::EXIT_EARLY),
            JsValue::undefined(),
            JsValue::null(),
        );

        // TODO: Here should be https://tc39.es/ecma262/#sec-globaldeclarationinstantiation

        self.realm().resize_global_env();

        Ok(())
    }

    pub(super) fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    pub(super) fn get_source(&self) -> SourceText {
        self.inner.source_text.clone()
    }
}
