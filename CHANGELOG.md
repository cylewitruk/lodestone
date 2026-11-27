# Changelog

User-facing changes per release. Measurements and design decisions are in
[docs/decision-log.md](docs/decision-log.md).

## [Unreleased]
## [0.1.5](https://github.com/cylewitruk/lodestone/compare/v0.1.4...v0.1.5) - 2026-11-27

### ✨ Added

- *(cli)* add lode doctor ([#58](https://github.com/cylewitruk/lodestone/pull/58))
- *(graph)* add JS, Go, Python, Maven, Gradle and .NET build graph providers ([#57](https://github.com/cylewitruk/lodestone/pull/57))
- *(transcripts)* read Antigravity transcripts in lode transcript-stats ([#54](https://github.com/cylewitruk/lodestone/pull/54))
## [0.1.4](https://github.com/cylewitruk/lodestone/compare/v0.1.3...v0.1.4) - 2026-11-26

### ✨ Added

- add lode upgrade and a new-version notice ([#52](https://github.com/cylewitruk/lodestone/pull/52))
## [0.1.3](https://github.com/cylewitruk/lodestone/compare/v0.1.2...v0.1.3) - 2026-11-25

### ✨ Added

- *(agents)* install into Google Antigravity ([#49](https://github.com/cylewitruk/lodestone/pull/49))
- add lode unregister (rm) command ([#48](https://github.com/cylewitruk/lodestone/pull/48))
## [0.1.2](https://github.com/cylewitruk/lodestone/compare/v0.1.1...v0.1.2) - 2026-11-22

### 🐛 Fixed

- address v0.1.1 review findings across install, relay, index and watcher ([#47](https://github.com/cylewitruk/lodestone/pull/47))

### ♻️ Changed

- count grep nudges and the searches that follow them ([#44](https://github.com/cylewitruk/lodestone/pull/44))

## [0.1.1](https://github.com/cylewitruk/lodestone/compare/v0.1.0...v0.1.1) - 2026-11-20

### Changed

- nudge at a guessed-name grep; stats follow shell commands ([#41](https://github.com/cylewitruk/lodestone/pull/41))
- apply global git excludes relative to the indexed checkout ([#40](https://github.com/cylewitruk/lodestone/pull/40))

## [0.1.0] - 2026-11-16

First release.

### Added

- `lode`: one daemon per machine that indexes any number of repositories and worktrees, keeps
  them fresh from file events, and serves search over MCP (`lode mcp`) and HTTP.
- Definition-level indexing with tree-sitter for Rust, Python, TypeScript/JavaScript, Svelte,
  Go, Java, C, C++, C#, Ruby, PHP, Lua, Elixir, Swift, Clarity and Markdown, plus any grammar
  configured for tree-sitter.
- Local embeddings with jina-embeddings-v2-base-code: candle on Metal (macOS) or the CPU;
  fastembed (`--features fastembed`) and OpenAI-compatible remote endpoints as alternatives.
- MCP tools `semantic_code_search`, `similar_code_search` and `status`.
- `lode install` for Claude Code and Codex: skill, MCP server and session reminder hooks.
- `lode transcript-stats`: how agents use lodestone, from their transcripts.

