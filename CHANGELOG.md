# Changelog

User-facing changes per release. Measurements and design decisions are in
[docs/decision-log.md](docs/decision-log.md).

## [Unreleased]

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

