# tree-sitter-proto highlight query

The built-in editor's tree-sitter integration (`gpui-component`) links the protobuf grammar from [`tree-sitter-proto`](https://github.com/coder3101/tree-sitter-proto) 0.2.0 but registers its language config without a highlight query, so a `.proto` buffer would parse and paint nothing. The crate does not export the query, so a verbatim copy of its `queries/highlights.scm` is vendored here and installed over the bundled config by `src/editor/highlighting.rs`.

Source: `tree-sitter-proto` 0.2.0 on crates.io. License: MIT (`LICENSE`, © 2024-2025 Mohammad Ashar Khan).

Keep the copy in step with the grammar: a pattern naming a node the grammar does not have makes the whole query fail to compile, and the editor silently falls back to plain text. `src/editor/highlighting.rs` pins the highlight output with a test, so a grammar update that breaks the query fails there rather than shipping unhighlighted.
