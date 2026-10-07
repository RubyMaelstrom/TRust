mp4parse 0.17.0, copied from the crates.io release (upstream commit 412bc177)
without dependency updates.

Why a patched copy: TRust decodes AVIF with this parser, but neither the
release nor mozilla/mp4parse-rust master (95d98b9a, 2026-09-22) exposes grid
derived image items (upstream issue #198) or the values of a clean aperture
('clap'); an essential clap even makes the item unprocessable. Encoders emit
grids for large images and clap for odd-sized crops, so TRust carries:

- `clap` is parsed into `CleanAperture` (ISOBMFF 12.1.4) and reported as a
  supported feature; `AvifContext::clean_aperture` returns it.
- `grid` primary and alpha items are kept. Their ordered 'dimg' inputs must be
  av01 items with av1C (and ispe outside permissive mode) and without
  unsupported essential properties. `AvifContext::{primary,alpha}_item_grid`
  parse the ImageGrid (HEIF 6.6.2.3), check the reference count, equal tile
  'ispe' and output coverage, and return the coded tiles in row-major order.
  `*_item_coded_data` stay `None` for grids. New `Status` values:
  GridBadDescriptor, GridBadTileCount, GridTileType.
- Safe accessors for Rust callers: `spatial_extents` (with
  `ImageSpatialExtentsProperty::{width,height}`), `nclx_colour_information`
  (with getters on `NclxColourInformation`) and `image_mirror`, instead of the
  raw-pointer C-API accessors.
- Three `'_` lifetime annotations that current rustc warns about.
- `u32::MAX`/`u64::MAX` replace the module constants `std::u32::MAX` and
  `std::u64::MAX`, deprecated by rustc 1.99, in `parse_mdhd`, `read_mvhd` and
  `read_mdhd`; test-only uses are unchanged. Upstream made the same change in
  8d6e19e8 (2024-05-01), after 0.17.0; no later crates.io release exists.

The upstream integration tests (tests/public.rs) need test files that the
crates.io package excludes, and still expect clap and grid to be unsupported.
TRust's AVIF regressions live in src/img/avif.rs.
