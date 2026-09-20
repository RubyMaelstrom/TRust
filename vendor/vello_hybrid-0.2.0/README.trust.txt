TRust owned fork of vello_hybrid 0.2.0 from crates.io.
Upstream revision: 7073a85d61ee099d1d3595651ddf92302b074348

The existing fork updates the renderer to wgpu 30 for panic-safe surface
teardown. The CSS Filter Effects 1 implementation additionally carries a
4-by-5 color matrix in the shared sparse shader filter packet. It uses the
same straight-alpha sRGB, per-operation clamping contract as vello_cpu.

Related owned forks: vello_common, vello_cpu, vello_sparse_shaders, all 0.2.0.
Their README.trust.txt files describe the shared filter addition.
