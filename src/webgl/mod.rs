//! WebGL over the system EGL/OpenGL ES driver. The browser-facing validation,
//! resource ownership and shader preparation are Rust; the driver is loaded on
//! the first context request. No ANGLE or bundled native compiler is used.
//!
//! Normative sources: Khronos WebGL 1.0, local snapshot 3b7a7538 (2026-09-06),
//! https://registry.khronos.org/webgl/specs/latest/1.0/; OpenGL ES 2.0.25;
//! GLSL ES 1.00.17; EGL 1.5 §§3.5 and 3.7. Contexts belong to the page actor,
//! while the canonical DOM owns the presentation bitmap consumed by both UIs.

mod context;
mod driver;
mod objects;
mod shader;
mod textures;
pub(crate) use context::{Attributes, Context, Reply};
pub(crate) const PAGE_BUDGET: usize = 256 * 1024 * 1024;
