# Lodestone — design and implementation plan

Lodestone is a local, centralized semantic code-search service for coding agents. One daemon per
machine indexes any number of repositories and worktrees, keeps the index fresh from file events,
and answers "what in this codebase does X?" over MCP in well under a second, with GPU (Metal)
embedding and code-tuned models.

This document is the brief for implementation. Section 1 is why; sections 2–7 are what; section 8
is the build order with acceptance criteria; section 9 is how it is measured.

## 1. Context and evidence

Built from a pilot on `stacks-core` (~694k lines of Rust, 796 files) measuring how often a coding
agent reuses an existing helper instead of re-implementing it. Numbers that shape this design:

| Finding | Number | Consequence |
| ---- | ---- | ---- |
| Agents don't search unless told | stock Sonnet reused 43%; a "search first" rule 70% | search must be cheap to *invoke*; never rely on the agent discovering a tool |
| Content in context beats content on disk | catalogue in prompt 85%; same catalogue as a file 70% | results must come back as short text the agent reads inline |
| Name-mismatch is the residual | 4/18 helpers unfindable by grep (`RetryReader`, `bounded_format`, …) | semantic retrieval is the right tool for this band, and only this band |
| Semantic search finds what grep can't | `ck` surfaced 2/4 of those on first query | retrieval quality matters more than any other component |
| Tool surface is paid every turn | 8 MCP tools ≈ 3.6k prompt tokens, re-read per turn | expose **two** tools |
| Verbose results are paid per call | `ck` JSON ≈ 4.9k chars/call vs ≈ 0.4k for grep | terse line-per-item output, relative paths |
| Steady-state query latency | 0.2 s via MCP; 9 s from CLI (process + model start) | resident model in a daemon |
| CPU indexing is slow | 74,574 chunks / 1,463 files = 30 min on `bge-small` CPU | GPU embedding + content-addressed cache |
| `ck` 0.8.1 failure modes | re-indexes on query (CLI/MCP key mismatch; any `include_patterns`); concurrent sessions corrupt the index | queries never index; one canonical path key; single writer per index |

The evaluation sets from that pilot live in `eval/` and are the acceptance bar for every phase.

## 2. Goals and non-goals

Goals:

- Index a 1M-line Rust workspace in under 10 minutes cold on an Apple-silicon GPU; seconds for a
  second worktree of the same repo; sub-second incremental update after an edit.
- Hybrid (semantic + lexical) search returning *items* (functions, types, macros) with signature and
  doc line, in ~0.2 s, over MCP and HTTP.
- Correct under many concurrent agent sessions and many worktrees.
- Measurable: ships with the eval harness and a `bench` subcommand.

Lodestone is universal. `stacks-core` is the example repository the evidence and the evaluation sets
come from; nothing in the codebase may assume it — no hard-coded crate names, paths, layer orders
or helper locations. Anything repository-specific (project graph, ignore rules, layer order) is
derived from the repository at index time or comes from that repository's own config, never from
lodestone's source. The eval corpus is an input to `lode bench`, not a dependency.

Non-goals (v1):

- Language-specific code of any kind. Languages arrive as tree-sitter grammars and queries (4.3);
  v1 compiles in Rust, Python, TypeScript/JavaScript, Go and Markdown and loads anything else from
  the user's tree-sitter parser directories. Build-system knowledge arrives as `ProjectGraph`
  providers (4.4); v1 ships Cargo and a fallback.
- Cloud sync, multi-user, auth. Loopback only.
- Call graphs, type inference, "ask the codebase" chat. This is retrieval.
- A UI.

## 3. Architecture

```text
 agents (Claude Code, others)            scripts / CI
        │  MCP streamable HTTP                  │  REST
        ▼                                       ▼
 ┌───────────────────────────── lodestone daemon ─────────────────────────────┐
 │  api/      two MCP tools + /v1 REST            (axum + rmcp)               │
 │  registry/ worktrees: canonical path → index handle, project graph         │
 │  watch/    notify per worktree, debounce, git HEAD change → rescan         │
 │  index/    per worktree: manifest, vector index (usearch), lexical (tantivy)│
 │  chunk/    tree-sitter-tags (any grammar) → definitions; prose → sections  │
 │  embed/    Embedder trait: candle+Metal | fastembed/ort | remote API       │
 │  cache/    global content-addressed chunk→embedding store (sqlite)         │
 │  search/   embed query → kNN + BM25 → RRF → collapse to items → layer      │
 │            filter → (rerank) → format                                      │
 └────────────────────────────────────────────────────────────────────────────┘
          ~/.lodestone/{config.toml, cache.sqlite, worktrees/<id>/...}
```

Single binary `lode` with subcommands; `lode serve` runs the daemon; other subcommands talk to it
over HTTP if it is running, else run in-process (so `lode search` works without a daemon, just
slower on first call).

### 3.1 Invariants

1. **Queries never index.** A search against a stale or missing index returns results from what
   exists plus a `stale: true` flag; `status` says why. Indexing happens only on `index`,
   `register`, and watcher events.
2. **One canonical key.** Every path is canonicalized (`std::fs::canonicalize`) once at the
   registry boundary; manifests store paths relative to the worktree root; the worktree id is
   `blake3(canonical_root)`. No other normalization anywhere.
3. **Single writer, snapshot readers.** Each worktree index has one async mutex for writes. Readers
   hold an `Arc` snapshot (`arc_swap::ArcSwap`) of the searchable state; a write builds the next
   snapshot and swaps it. A search never blocks on an index update and never observes a half-written
   index.
4. **Embeddings are a pure function of (model id, chunk text).** The cache key is
   `blake3(model_id ‖ chunk_text)`. Nothing else may go into that key.
5. **Scope filters never touch index state.** Unit, language, path prefix and kind filters are applied
   to results only.

## 4. Data model

### 4.1 Global cache (`~/.lodestone/cache.sqlite`)

```sql
CREATE TABLE embeddings (
  key       BLOB PRIMARY KEY,   -- blake3(model_id || chunk_text), 32 bytes
  model_id  TEXT NOT NULL,
  dim       INTEGER NOT NULL,
  vector    BLOB NOT NULL,      -- f32 little-endian, dim * 4 bytes
  created   INTEGER NOT NULL    -- unix seconds
);
CREATE TABLE models (model_id TEXT PRIMARY KEY, dim INTEGER NOT NULL, revision TEXT);
```

Shared across every repository and worktree on the machine. Second worktree of the same repo:
chunk + hash + lookup only, zero embedding work for unchanged text. Prune by `created` age if it
grows past a configurable size.

### 4.2 Per-worktree index (`~/.lodestone/worktrees/<id>/`)

```text
manifest.json     { root, model_id, files: { "rel/path.rs": { file_hash, mtime, size, chunk_ids: [...] } } }
items.sqlite      items(id, rel_path, language, kind, name, qualified_name, line_start, line_end, signature, doc, unit, is_test, chunk_key)
vectors.usearch   usearch HNSW index, key = item id, cosine, f32 (or f16 quantized; decide in spike)
lexical/          tantivy index: fields rel_path, unit, language, name, signature, doc, body (tokenized for code)
graph.json        project unit graph from the matched providers (see 4.4)
```

`items.sqlite` is the join table between the two indexes and the source of truth for output lines.

### 4.3 Chunk = definition (language-agnostic)

Lodestone has no language-specific code. The unit of indexing is a **definition** as tree-sitter's
tagging convention identifies it: every grammar ships `queries/tags.scm` whose captures
(`@definition.function`, `@definition.class`, `@definition.method`, `@definition.module`, …,
plus `@name` and `@doc`) say what the definitions are and where their doc comments sit. The
`tree-sitter-tags` crate evaluates those queries and yields `(kind, name, range, docs)`; that output
is the chunker's only input. Adding a language is adding a grammar and its queries, never Rust code.

Per definition:

| Field | Source |
| ---- | ---- |
| `kind` | the tag kind from `tags.scm` (`function`, `method`, `class`, `module`, `interface`, `type`, `constant`, `macro` — whatever the grammar defines) |
| `name`, `qualified_name` | `@name`; qualified with the enclosing definition's name when the tag is nested (method in class/impl, fn in mod) |
| `doc` | `@doc` capture (grammars strip comment markers via `#strip!`); first line is what the output shows |
| `chunk text` | one-line header `<rel_path> <kind> <qualified_name>` + doc + the definition's source, truncated to ~300 tokens; for container kinds (class, impl, module) the header + doc + member signatures only, so a 2,000-line class is not one blob |
| `is_test` | the definition or an ancestor matches the language's test marker query (see below) |

Two small optional per-language queries extend the convention where `tags.scm` is not enough:

- `signature.scm`: how to extract a one-line signature (default: the definition's first line up to
  the body opener).
- `tests.scm`: nodes that mark test code (Rust `#[cfg(test)]`/`#[test]`, Python `test_*`,
  JS `describe(`…). Absent → nothing is marked as test.

Files with no grammar are chunked by blank-line-separated paragraphs of ~300 tokens with
`kind = text` (Markdown additionally splits on headings). Nothing is skipped silently; `status`
reports per-extension counts of `tagged` vs `text` chunks.

**Language registry.** Built-in grammars are compiled in behind features (`lang-rust`,
`lang-python`, `lang-typescript`, `lang-go`, `lang-markdown`, …) using each `tree-sitter-<lang>`
crate's exported `TAGGING_QUERY`/`LOCALS_QUERY`. Beyond those, `tree-sitter-loader` loads any
grammar the user has cloned into the `parser-directories` of tree-sitter's own
`~/.config/tree-sitter/config.json`, reading the grammar's `tree-sitter.json` for file types and its
`queries/` for tags. Lodestone's config only maps extensions to language names when the grammar's
own metadata doesn't.

### 4.4 Project graph and layers (provider-based)

The layer filter needs "which units may this unit depend on". That is build-system knowledge, so
it lives behind a `ProjectGraph` provider trait: `fn units(root) -> Vec<Unit{name, paths}>` and
`fn deps(unit) -> Vec<unit>`. Providers are detected from marker files and may stack
(a monorepo can hold several):

| Provider | Marker | Units | Edges |
| ---- | ---- | ---- | ---- |
| Cargo | `Cargo.toml` with `[workspace]` or `[package]` | packages | `[dependencies]`, `[dev-dependencies]`, `path`/`workspace = true` |
| npm/pnpm/yarn workspaces | `package.json` with `workspaces` | packages | `dependencies`, `devDependencies` to sibling names |
| Go | `go.work` / `go.mod` | modules | `require` to sibling module paths |
| Python (uv/poetry/hatch workspaces) | `pyproject.toml` | projects | `dependencies` to sibling names |
| fallback | none | one unit = the repository | no edges |

v1 ships Cargo plus the fallback; the others are additions behind the same trait. The graph is
stored in `graph.json`; `search` takes `from_unit`. With no provider matched, the filter is a
no-op and `status` says so. (The Cargo parsing heuristics in `eval/harness/gen_slices.py` are a
reference, not code to port.)

## 5. Pipelines

### 5.1 Indexing

```text
walk(root)  → ignore crate (respects .gitignore + .lodestoneignore + extension allowlist)
  for each file whose (size, mtime) or blake3 changed since manifest:
    chunk(file) → [Item]
    for each item: key = blake3(model_id || text)
      cache hit  → vector
      cache miss → queue for embedding
  embed queued texts in batches of 64 (GPU) / 16 (CPU) → write cache
  swap file's items in items.sqlite, vectors, lexical (delete old ids, insert new)
  update manifest entry
  removed files → delete their items
build/refresh snapshot → ArcSwap
```

Cold index is the same loop with an empty manifest. Progress is reported on `status` as
`{files_done, files_total, chunks_embedded, chunks_cached, eta}`.

### 5.2 Freshness

- `notify` recursive watcher per registered worktree, 500 ms debounce, coalesced per path.
- Ignore events under `target/`, `.git/` (except `HEAD` and `refs/`), and ignored paths.
- On `.git/HEAD` or `refs/` change (branch switch, rebase): full manifest rescan (cheap: hashes
  only; embedding only for genuinely new text thanks to the cache).
- Watcher failure (e.g. too many open files) degrades to a 60 s poll of manifests with a warning in
  `status`; it never silently stops.

### 5.3 Search

```text
q = embed(query)                                 (resident model, ~5 ms on GPU)
V = usearch.search(q, k = 200)                   (cosine)
L = tantivy.search(query, k = 200)               (BM25 over name+signature+doc+body)
R = rrf(V, L, k = 60)                            (reciprocal rank fusion)
R = filter(R, from_unit, path_prefix, language, kinds, include_tests)
R = collapse(R)                                  (one row per item; keep best score)
R = rerank(R[..50])  if reranker enabled         (cross-encoder, phase 3)
return R[..top_k] formatted
```

Query-time extras (cheap, measurable, do each only if the eval shows a gain):

- If the query contains an identifier-like token (`snake_case`, `CamelCase`, `::`), boost lexical
  weight.
- Identifier hits that match the query verbatim rank first regardless of embedding score.

### 5.4 Output format

One line per item, nothing else. This format is the product; keep it stable.

```text
stacks-common/src/util/retry.rs:19  struct RetryReader<'a, R: Read>  — Wrap a Read so that we store a copy of what was read
stacks-common/src/util/mod.rs:212  fn bounded_format<T: Debug>(x: &T, n: usize) -> String  — Debug-format x to at most n bytes with an ellipsis
```

`rel_path:line  kind qualified_name(signature) — first doc line`. Signature truncated at 120 chars,
doc at 100. Optional `--with-body N` adds N lines of the item for REST callers; the MCP tool never
includes bodies by default.

## 6. Interfaces

### 6.1 MCP (streamable HTTP, `POST /mcp`)

Exactly two tools. Descriptions are part of the design; the agent reads them on every turn.

```json
{
  "name": "search",
  "description": "Find existing code by what it does. Describe the behaviour in plain words (not a guessed name); returns matching functions, types and macros as 'path:line kind name(signature) — doc'. Use before writing any helper.",
  "input": {
    "query":      "string, required",
    "repo":       "string, optional: worktree root or registered name; default = the only registered repo, else error",
    "from_unit":  "string, optional: only items this package/crate/module may depend on, per the repo's build graph",
    "language":   "string, optional: restrict to one language",
    "top_k":      "integer, optional, default 8, max 50",
    "include_tests": "boolean, optional, default false"
  }
}
```

```json
{
  "name": "status",
  "description": "Index freshness for a repo: files indexed, pending changes, model, last update.",
  "input": { "repo": "string, optional" }
}
```

Both return plain text (the format in 5.4; `status` returns key: value lines). No pagination, no
JSON envelope, no cursors.

### 6.2 REST (`/v1`)

`GET /v1/search?q=&repo=&from_unit=&language=&top_k=&format=text|json`, `GET /v1/status?repo=`,
`POST /v1/register {path}`, `POST /v1/index {repo, force}`, `GET /v1/repos`. JSON for scripts;
same engine.

### 6.3 CLI

```text
lode serve [--port 4600] [--config ~/.lodestone/config.toml]
lode register <path>            # canonicalize, add to registry, start watcher, index if needed
lode index <path> [--force]     # cold or incremental, prints progress
lode search "<query>" [--repo] [--from-unit] [--language] [--top-k] [--json]
lode status [--repo]
lode bench <eval-dir> [--model …]   # rank@k on eval/questions.json, index time, query latency
lode models                     # list configured/available embedding models and backends
lode languages                  # built-in grammars, grammars found via tree-sitter config, extension map
```

### 6.4 Config (`~/.lodestone/config.toml`)

```toml
port = 4600
model = "jina-code-v2"          # alias into [models]
backend = "candle-metal"        # candle-metal | candle-cpu | fastembed | remote
batch_size = 64
cache_max_gb = 4

[languages]
# Grammars compiled in via `lang-*` features need nothing here. External grammars are discovered
# through tree-sitter's own config (~/.config/tree-sitter/config.json → parser-directories) and
# their tree-sitter.json / queries/. Entries here only override or add extension mappings.
tree_sitter_config = "~/.config/tree-sitter/config.json"
extensions = { "cljc" = "clojure" }
index_untagged_as_text = ["md", "txt", "adoc"]   # extensions chunked as prose when no grammar matches

[models.jina-code-v2]
hf_repo = "jinaai/jina-embeddings-v2-base-code"
dim = 768
max_tokens = 512

[rerank]
enabled = false
model = "bge-reranker-v2-m3"
```

Agent wiring (Claude Code `.mcp.json`): `{"mcpServers": {"lodestone": {"type": "http", "url": "http://127.0.0.1:4600/mcp"}}}`.

## 7. Crates (verified 2026-11-07 via rust-mcp; re-check before pinning)

| Crate | Version | Purpose | Notes |
| ---- | ---- | ---- | ---- |
| `tokio`, `axum` | current | runtime, HTTP | |
| `rmcp` | 3.9.0 | MCP server | enable `transport-streamable-http-server`; `server` + `macros` are default |
| `tree-sitter` | 0.27.x | parsing | |
| `tree-sitter-tags` | 0.27.2 | definitions from `tags.scm` (`TagsConfiguration::new(language, TAGGING_QUERY, LOCALS_QUERY)`) | the whole chunker rests on this |
| `tree-sitter-loader` | 0.27.2 | load user grammars at runtime from tree-sitter's `config.json` parser directories | |
| `tree-sitter-rust` 0.24.2, `-python`, `-typescript`, `-javascript`, `-go`, `-md` | current | built-in grammars, each behind a `lang-*` feature | each exports its tagging query constant; check the name per crate |
| `candle-core`, `candle-nn`, `candle-transformers` | 0.11.1 | embedding runtime | feature `metal` exists on `candle-core`; models load from safetensors via `hf-hub` |
| `tokenizers` | current | tokenizer for candle path | |
| `fastembed` | 7.6.1 | fallback runtime (ONNX via `ort`) and rerankers | sync API, no tokio dependency; use for the spike baseline and for CPU/Linux |
| `usearch` | 2.26.6 | HNSW vector index | single-file, f16 option |
| `tantivy` | current | BM25 | custom tokenizer that splits `snake_case`/`CamelCase` |
| `rusqlite` (bundled) | current | cache + items | WAL mode |
| `notify`, `notify-debouncer-full` | current | file events | |
| `ignore` | current | gitignore-aware walking | same crate `ripgrep` uses |
| `blake3`, `arc-swap`, `serde`, `toml`, `clap`, `tracing` | current | | |

Verify each with `rust-mcp` (`crate_intel`, `crate_features`, `crate_api`) at the start of the
phase that introduces it; do not pin from this table without checking.

## 8. Phases

Each phase ends with its acceptance criteria met and `cargo nextest run` green. Keep a `CHANGELOG.md`
entry per phase with the measured numbers.

### Phase 0 — Scaffold and spike (≈ half a day)

- Cargo workspace: single crate `lodestone`, binary `lode`, modules per section 3.
- `Embedder` trait + two backends behind features: `fastembed` (works everywhere, gets a baseline
  on day one) and `candle` with `metal`.
- Chunker on `tree-sitter-tags` with the Rust and Python grammars; confirm `@doc` captures come
  through for both and that a grammar loaded via `tree-sitter-loader` (clone one not compiled in,
  e.g. tree-sitter-c) tags a sample file with no code changes.
- Spike: embed 10k chunks from `eval/corpus` with each backend × 2–3 models; record
  chunks/s and peak memory. Run `lode bench` rank@5 on `eval/questions.json` per model.
- **Accept:** a table in `CHANGELOG.md` of model × backend × chunks/s × rank@5; one model and
  backend chosen by it; `candle-metal` at least 5× `fastembed` CPU or documented why not.

### Phase 1 — Index and search, in-process (≈ 1 day)

- Chunker (4.3) with the five built-in grammars plus loader, cache (4.1), per-worktree index
  (4.2), project graph with Cargo and fallback providers (4.4), indexing loop (5.1), search (5.3
  without rerank), output (5.4), CLI `index`/`search`/`status`/`bench`/`languages`.
- **Accept:** cold index of `stacks-core` < 10 min on Metal; re-index with no changes < 5 s;
  a mixed-language repository (e.g. one with Rust, TypeScript and Python) indexes with every
  extension accounted for in `status` and definitions tagged per language;
  second worktree of the same commit < 30 s; `lode search` p50 < 300 ms warm (in-process, model
  resident for the process lifetime); rank@5 ≥ 0.85 on `eval/questions.json` `easy`+`mismatch`,
  and all four hard helpers (`RetryReader`, `bounded_format`, `guarded_string`,
  `select_transactions_where`) in the top 5 for their `eval/questions.json` queries.

### Phase 2 — Daemon, watcher, MCP (≈ 1 day)

- `lode serve`: registry, ArcSwap snapshots, single-writer locks, watcher (5.2), REST (6.2), MCP
  (6.1), CLI falls back to HTTP when the daemon is up.
- **Accept:** 8 concurrent clients × 200 searches with a watcher-triggered reindex running
  underneath: zero errors, zero stale-read panics, p50 < 250 ms, p99 < 1 s; edit a file → item
  searchable within 2 s; branch switch → fully fresh within 30 s with ≥ 95% cache hits; killing
  the daemon mid-index and restarting leaves a consistent index (manifest written last, per file);
  Claude Code connects via `.mcp.json` and the two tools appear with the exact descriptions in 6.1.

### Phase 3 — Quality (≈ half a day, gated)

- Reranker (cross-encoder over top 50; `fastembed` rerankers are the quick path), identifier
  boosting, test down-weighting, `module` chunks.
- **Accept:** each change is kept only if it raises rank@5 on `eval/questions.json` or hard-case
  hit rate, and does not push p50 above 400 ms with rerank on.

### Phase 4 — End-to-end agent eval (≈ half a day)

- Port `eval/harness/run_trials.py` to an arm `rule_lode` (reuse rule + mandatory `search` call,
  MCP config pointing at the daemon). Run the 18 tasks × 3 reps on Sonnet.
- **Accept:** a results table alongside the pilot's (`stock` 23/54, `rule` 38/54,
  `rule_cat_prompt` 46/54, `rule_skill` 42/54, forced `ck` 1/4 hard cases). Report cost per run and
  tokens; the target is ≥ 3/4 hard cases and cost within 1.3× of `rule_cat_prompt`.

## 9. Evaluation assets (`eval/`)

| Path | What |
| ---- | ---- |
| `questions.json` | 24 retrieval questions against `stacks-core` main `95c6a45eed`: `easy` (4), `mismatch` (16: concept ≠ name), `negative` (4: no helper exists). Schema `{id, kind, question, answers:[{path,symbol,line}], …}` |
| `tasks.json` | 18 implementation tasks with `accept` regexes; used by the agent-level harness |
| `harness/run_trials.py`, `score.py` | headless `claude -p` runner and grader from the pilot |
| `harness/gen_catalogue.py`, `gen_tree.py`, `gen_slices.py` | the static-catalogue generators (reference for crate-graph parsing and item extraction heuristics) |
| `harness/ck_mcp.py` | minimal stdio MCP client used to probe `ck`; adapt for HTTP if useful |

`lode bench <eval-dir>` takes the corpus path and question file as arguments and must compute, per
question: rank of the first correct answer (by `path` and `symbol`) in the top 50, and report
rank@1/@5/@10, mean reciprocal rank, and for `negative` questions the score gap (top hit score
should be low; tune a threshold from these). The corpus is a clean clone of the example repository
at the commit above; `eval/README.md` says how to fetch it. The question-file schema is generic so a
second repository's set can be added without code changes (see Risks, eval overfitting).

## 10. Coding standards

- Rust 2024 edition, stable toolchain; `cargo fmt --all`, `cargo clippy --all-targets -D warnings`.
- Tests with `cargo nextest run`; use `std::assert_matches!` for match asserts.
- No `pub(crate)` or `pub(super)`: design module boundaries and re-exports so visibility is `pub` or
  private.
- Module-level `use` imports; for modules with many free functions import the module
  (`use std::mem;`), not each function.
- Doc comments say what and the non-obvious why, briefly. No narrative.
- Every public type in `api/` has a serde round-trip test; every invariant in 3.1 has a test that
  would fail if it were violated (e.g. a search during an in-progress reindex returns the old
  snapshot; a query with filters leaves the manifest byte-identical).

## 11. Open decisions (settle in Phase 0 by measurement)

| Decision | Options | Criterion |
| ---- | ---- | ---- |
| Embedding runtime | candle+Metal vs fastembed/ort (CoreML EP) | chunks/s on 10k chunks; must beat 2,000 chunks/min |
| Model | jina-embeddings-v2-base-code, nomic-embed-code, Qodo-Embed-1-1.5B, bge-small (baseline) | rank@5 on `questions.json`, then speed |
| Vector precision | f32 vs f16 in usearch | rank@5 unchanged within 0.02 → f16 |
| Body truncation | 200 / 300 / 500 tokens per item | rank@5; shorter is cheaper |
| Lexical tokenizer | tantivy default vs custom code tokenizer | hit rate for identifier queries |
| Built-in grammar set | which `lang-*` features are default-on | binary size and build time vs. covering the user's repos without a tree-sitter checkout |

## 12. Risks

- **Model download size and licensing.** Check each model's license before defaulting to it;
  cache under `~/.lodestone/models`.
- **Metal kernel coverage in candle for a given architecture.** If a model's ops fall back to CPU,
  the spike will show it; prefer a model that runs fully on Metal over a marginally better one that
  doesn't.
- **Tagging-query quality varies by grammar.** Some `tags.scm` files lack `@doc` captures or
  miss definition kinds (e.g. Rust's does not tag `macro_rules!` or `const` in every version).
  Mitigation: lodestone may ship *supplementary* `tags.scm` overlays per built-in language under
  `queries/<lang>/`, merged after the grammar's own; still queries, never code. Generated items
  (macro expansions) stay invisible; accept for v1.
- **Grammar ABI drift.** Grammars loaded at runtime must match the `tree-sitter` ABI range the
  binary was built with; `lode languages` reports incompatible ones instead of failing the index.
- **Watcher limits.** macOS FSEvents is fine; on Linux inotify limits need a documented
  `max_user_watches` note.
- **Eval overfitting.** The 24 questions were written against stacks-core; add 10–20 questions from
  a second repository in Phase 3 before trusting any ranking tweak.
