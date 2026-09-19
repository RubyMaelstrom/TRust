glsl-lang-pp 0.8.1, from the crates.io release at upstream 247fbba98cd1ba9e1c87f4d5a8e0568fbdc48cfe.

TRust changes: bound eager macro substitution before allocating intermediate
vectors (100,000 token visits per expansion, 64 nested macros, and 1 MiB
of token-paste input). Shader-facing errors preserve normal preprocessing;
resource exhaustion rejects compilation instead of expanding without a bound.
The browser also limits total source, post-preprocessing tokens, and AST depth.
See src/webgl/shader.rs for regression cases and WebGL/GLSL references.

Also bound recursive #if/#line evaluation to 1,024 tokens and 64
parentheses; spell elided return lifetimes explicitly for current Rust.
