TRust's owned usvg 0.48.1 fork
============================

Base: the unmodified crates.io usvg 0.48.1 package (MIT OR Apache-2.0),
checksum 977d0a4abdef933f424a99fe09f95576e089b90aebc6f016a3bc813762493e91.
The original license files and upstream README are preserved.

Options::retain_text_clusters (default false) retains optional
Span::positioned_clusters for browser SVG DOM geometry queries. Each record
has the source chunk and logical UTF-8 cluster range before bidi reordering,
the formatted advance, and the full glyph-cell rectangle with its positioned
transform. These values come from the shaping/layout pipeline; consumers do
not reconstruct cluster positions from neighboring glyphs or font guesses.
Whitespace-only Text nodes remain available when this option is enabled even
when flattening yields no outlines. Ordinary SVG image rendering keeps the
option disabled and does not allocate the retained vectors.

SVG 2 text.html#__svg__SVGTextContentElement__getExtentOfChar and
coords.html#TermFullGlyphCell require character geometry from these cells.
The horizontal cell excludes added letter/word spacing. Sideways vertical
text rotates that cell; upright vertical cells are one EM wide with the
font's vertical advance as height, or one EM high when no vertical metrics
exist. Vertical advances are shaped once per font/configuration per chunk,
including variable-font settings and automatic optical sizing. Upright cells
follow the writing direction separately from the glyph-outline orientation.

The fork does not change existing SVG raster layout, shaping or painting.
TRust maps the logical ranges to addressable UTF-16 units and applies the
tight rectangle bounds in src/dom/svg_geometry.rs. Focused browser tests are
in src/svg_text_extent_tests.rs and src/fixtures/svg_text_extent.mjs.

Consulted SVG 2 snapshot c403ca46ad045ebdaeda148bae69f814fb744db7
(2026-09-06), plus OpenType vmtx/VVAR table definitions:
https://www.w3.org/TR/SVG2/text.html#__svg__SVGTextContentElement__getExtentOfChar
https://www.w3.org/TR/SVG2/coords.html#TermFullGlyphCell
https://learn.microsoft.com/en-us/typography/opentype/spec/vmtx
https://learn.microsoft.com/en-us/typography/opentype/spec/vvar
