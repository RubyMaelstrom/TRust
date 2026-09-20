TRust owned fork of vello_common 0.2.0 from crates.io.
Upstream revision: 7073a85d61ee099d1d3595651ddf92302b074348

Adds native color-matrix filter support for CSS Filter Effects 1.
Color operations use straight-alpha sRGB components, clamp each result, and
return premultiplied pixels. CPU and Hybrid share the same matrix contract.
The Hybrid shader packet grows to hold a complete 4-by-5 matrix.
No dependency version update is included.
