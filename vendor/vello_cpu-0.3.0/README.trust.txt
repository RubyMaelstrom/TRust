TRust owned fork of vello_cpu 0.3.0 from crates.io.
Upstream revision: f3000c8d9e7a9c7e6abb09b9587238bf8b4860ff
Rebased from the 0.2.0 fork on 2026-10-07.

Adds native color-matrix filter support for CSS Filter Effects 1.
Color operations use straight-alpha sRGB components, clamp each result, and
return premultiplied pixels. CPU and GPU share the same matrix contract.
The GPU shader packet grows to hold a complete 4-by-5 matrix.
No dependency version update is included.
