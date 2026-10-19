//! Coordinate synchronous native GPU backend initialization across API users.
//!
//! Vulkan's Object Lifetime and Threading Behavior clauses permit independent
//! instances on different threads. However, the 2026-10-19 native probe and
//! core dumps show an installed ICD publishing its initialization flag before
//! its dispatch pointer: concurrent extension enumeration and instance creation
//! can then call a null pointer. `tools/vulkan_init_probe.c` reproduces this
//! without TRust, wgpu, FFmpeg, or image rendering; the original evidence is
//! retained in target/servo-native-crash-20261019/.
//!
//! Share this gate between wgpu instance construction and FFmpeg Vulkan device
//! creation. Only the synchronous initialization call runs inside it; adapters,
//! wgpu device requests, surfaces, rendering, and decoding keep their ordinary
//! independent ownership and concurrency. No native resource is retained by
//! the gate.
//!
//! Standards: https://docs.vulkan.org/spec/latest/chapters/fundamentals.html
//! (#fundamentals-objectmodel-lifetime and #fundamentals-threadingbehavior).

use std::sync::Mutex;

static INITIALIZATION: Mutex<()> = Mutex::new(());

pub(crate) fn initialize<T>(create: impl FnOnce() -> T) -> T {
    let _guard = INITIALIZATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    create()
}
