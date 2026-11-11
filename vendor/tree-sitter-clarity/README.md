# tree-sitter-clarity (vendored)

Generated parser from [xlittlerag/tree-sitter-clarity](https://github.com/xlittlerag/tree-sitter-clarity)
at `f3b7520fa336e877fc7bb180902e325d465da052` (Clarity 6), MIT-licensed (`LICENSE`). Not on
crates.io, so `build.rs` compiles `src/parser.c` behind the `lang-clarity` feature.
`grammar.js` and `src/node-types.json` are kept as a reference for `queries/clarity/tags.scm`.

To update, copy `grammar.js`, `LICENSE`, `src/parser.c`, `src/node-types.json` and
`src/tree_sitter/*.h` from a newer commit, update the commit above, and run
`cargo nextest run clarity`.
