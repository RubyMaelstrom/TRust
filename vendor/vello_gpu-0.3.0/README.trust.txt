TRust owned fork of vello_gpu 0.3.0 from crates.io (formerly vello_hybrid).
Upstream revision: f3000c8d9e7a9c7e6abb09b9587238bf8b4860ff
Rebased from the vello_hybrid 0.2.0 fork on 2026-10-07. Upstream now targets
wgpu 30, so the fork's earlier wgpu 30 port is no longer carried.

TRust changes:
- CSS Filter Effects 1 color matrices: a 4-by-5 matrix in the shared sparse
  shader filter packet, using the same straight-alpha sRGB, per-operation
  clamping contract as vello_cpu.
- Pixmap image sources (bitmap glyphs and other CPU pixmap paints) are
  uploaded to transient image-atlas allocations for one render and released,
  encoder-ordered, after the draw.
- `Renderer::update_image` replaces a same-sized image's pixels in place for
  animation frames.
- Pixmap atlas uploads are recorded in the command encoder through a staging
  buffer (WebGPU §§3.4.1, 19.2), so they stay ordered with atlas clears and
  growth copies instead of overtaking them as queue writes.
- `Resources::image_atlas_count` exposes the image atlas count for TRust's
  atlas release and growth regression tests.

Related owned forks: vello_common, vello_cpu, vello_gpu_shaders (0.3.0) and
glifo (0.4.0). Their notes describe the shared filter and hinting additions.
