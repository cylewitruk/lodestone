# Contributing

## Build and test

The toolchain is pinned in `rust-toolchain.toml`; tests use [cargo-nextest](https://nexte.st).
CI runs:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo nextest run --locked
```

Tests use a deterministic hash embedder, so they need no model download. The external-grammar
test runs only with `LODESTONE_TEST_PARSER_DIR` set (see `tests/it/external_grammar.rs`).

## Code

- Rust 2024 edition; module-level `use` imports, importing the module for free functions
  (`use std::mem;`).
- Visibility is `pub` or private, shaped with modules and re-exports; no `pub(crate)`.
- `std::assert_matches!` for match asserts.
- Doc comments say what and the non-obvious why, briefly.
- Each [invariant](docs/architecture.md#invariants) has a test that fails if it is violated.

## Docs

- User-facing changes go in `CHANGELOG.md` (release-plz drafts it from commits).
- A changed default or a measured trade-off goes in [docs/decision-log.md](docs/decision-log.md).
- Retrieval or agent-behaviour changes are measured first; see
  [docs/evaluation.md](docs/evaluation.md).

## Releases

See [docs/releasing.md](docs/releasing.md).
