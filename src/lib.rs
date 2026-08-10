//! Shared TRust browser engine and frontend-independent application contracts.
//!
//! The terminal executable is now a client of this library, as is the native
//! desktop executable. Terminal presentation remains in [`app`] and [`ui`]
//! while the permanent cross-frontend boundary lives in [`core`] and
//! renderer-neutral display data lives in [`render`].

// mimalloc as the global allocator (default-on `mimalloc` feature): ~17%
// faster JS parse+compile, which are dominated by millions of tiny AST/
// CodeBlock allocations. `--no-default-features` falls back to the system
// allocator (pure Rust). See the feature note in Cargo.toml.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Return freed allocator memory to the OS at navigation boundaries.
pub fn release_allocator_memory() {
    #[cfg(feature = "mimalloc")]
    // SAFETY: `mi_collect` is a process-global mimalloc management entry point
    // with no preconditions; it only reclaims memory that is already unused.
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
}

pub mod accessibility;
pub mod command;
pub mod core;
pub mod render;
pub mod responsive_image;

// These are the existing engine modules. They are declared exactly once here;
// binaries import this library rather than compiling private copies with `mod`.
pub mod app;
pub mod cp437;
pub mod doc;
pub mod dom;
pub mod gemini;
pub mod gopher;
pub mod http;
pub mod img;
pub mod js;
pub mod layout2;
pub mod media;
pub mod oneshot;
pub mod telnet;
pub mod terminal_view;
pub mod text;
pub mod theme;
pub mod tls;
pub mod ui;
pub mod ws;

/// Stable frontend grouping. The root re-exports above remain while the
/// established terminal code is migrated incrementally instead of churned.
pub mod frontend {
    pub mod tui {
        pub use crate::app::{App, TERMINAL_OWNER, parse_port};
    }
}
