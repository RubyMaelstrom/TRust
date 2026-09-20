html5ever 0.39.0, copied from the crates.io release without dependency updates.

TRust patch: match template shadowrootmode keywords ASCII case-insensitively,
as required by HTML #keywords-and-enumerated-attributes and #parsing-main-inhead.
The embedding TreeSink implements declarative shadow attachment. Conformance
regressions, including mixed-case keywords, live in src/dom/shadow.rs in TRust.
