//! Thread-local configuration for TRust lazy *parsing* (skipping eligible
//! function bodies at parse time; see [`crate::lazy_scan`]).
//!
//! The engine (`boa_engine`/TRust) turns this on per page thread from the
//! `TRUST_LAZY_PARSE` flag. It is off by default, so an unconfigured parser
//! behaves exactly as before. The setting is thread-local because each TRust
//! page parses on its own thread, and the parallel-parse / image-cache paths
//! must keep it off (their interner cannot back a deferred body's re-parse).

use std::cell::Cell;

/// Default minimum body length (UTF-16 code units) worth skipping: below it the
/// stub + first-call re-parse can cost more than an eager parse, and a tiny
/// function instantiated in a loop would re-parse per closure.
pub const DEFAULT_MIN_LEN: usize = 100;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static MIN_LEN: Cell<usize> = const { Cell::new(DEFAULT_MIN_LEN) };
    static EAGER_NEXT_BODY: Cell<bool> = const { Cell::new(false) };
}

/// Mark the NEXT function body parsed on this thread to be parsed eagerly (not
/// skipped). Set by the parser when it is about to parse a *parenthesized*
/// function expression — the `(function(){…})(…)` IIFE / UMD wrapper, which runs
/// immediately, so skipping it would only force an instant re-parse on the
/// call. Its nested functions (the never-called module bodies) still skip. The
/// flag is one-shot, consumed by the next [`take_eager_next_body`].
pub fn set_eager_next_body() {
    EAGER_NEXT_BODY.with(|c| c.set(true));
}

/// Take (and clear) the one-shot eager-next-body flag. Called at the start of
/// every function-body parse, so a flag set before a non-skippable body (e.g. a
/// parenthesized generator) is cleared by that body rather than leaking.
pub(crate) fn take_eager_next_body() -> bool {
    EAGER_NEXT_BODY.with(|c| c.replace(false))
}

/// Enable or disable lazy parsing on the current thread. Off by default.
pub fn set_enabled(enabled: bool) {
    ENABLED.with(|c| c.set(enabled));
}

/// Whether lazy parsing is enabled on the current thread.
#[must_use]
pub fn enabled() -> bool {
    ENABLED.with(Cell::get)
}

/// Temporarily disable lazy parsing on the current thread, restoring the prior
/// state when the returned guard drops. Used by the engine to keep the prelude
/// and image-cached (CDN) compiles eager — a body skipped at parse time becomes
/// a lazy stub that those paths cannot dehydrate into a cached image.
#[must_use]
pub fn suppress() -> SuppressGuard {
    let prev = ENABLED.with(Cell::get);
    ENABLED.with(|c| c.set(false));
    SuppressGuard(prev)
}

/// Restores the lazy-parse enabled state on drop. See [`suppress`].
#[derive(Debug)]
pub struct SuppressGuard(bool);

impl Drop for SuppressGuard {
    fn drop(&mut self) {
        ENABLED.with(|c| c.set(self.0));
    }
}

/// Set the minimum body length (UTF-16 code units) eligible for skipping.
pub fn set_min_len(min_len: usize) {
    MIN_LEN.with(|c| c.set(min_len));
}

/// The minimum body length (UTF-16 code units) eligible for skipping.
#[must_use]
pub fn min_len() -> usize {
    MIN_LEN.with(Cell::get)
}
