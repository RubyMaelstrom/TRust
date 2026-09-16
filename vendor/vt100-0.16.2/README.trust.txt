TRust owned VT engine fork, based on vt100 0.16.2 (MIT).

Both frontends consume this engine through src/terminal.rs. Engine defects
are fixed here, not worked around in an individual painter. The original
license and authorship are retained. Conformance regressions live in TRust's
src/terminal.rs and src/terminal_view.rs so both adapters exercise the fork.

Scope: streaming terminal queries at the parser dispatch point; safe wide
cells and resizing; DEC character sets, wrap/tab/cursor modes; ECMA-48
rendition; bounded Unicode grapheme clusters. TRust's presentation adapters
render the same cells and use the same input-mode state.

References: ECMA-48 fifth edition (1991), especially 8.3.35 DSR, 8.3.63 HVP,
8.3.117 SGR; xterm control sequences, Patch 411 (2026-08-23); Unicode UAX 29
extended grapheme clusters. Focused TRust tests accompany each extension.
