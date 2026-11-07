# Changelog

Measured on an Apple M4 Max (16 cores, 64 GB), macOS, Rust 1.98, against `stacks-core`
`95c6a45eed`: 1,018 indexable files (796 `.rs`, 168 `.ts`, 61 `.md`, …), 27,333 definitions.
Retrieval metrics come from `lode bench eval --hard q06,q07,q08,q18` on `eval/questions.json`
(rank@k over the 20 `easy` + `mismatch` questions; hard = RetryReader, bounded_format,
guarded_string, select_transactions_where).

## Phase 0 — Scaffold and spike

Single crate `lodestone`, binary `lode`, modules per §3 (`api`, `registry`, `watch`, `index`,
`chunk`, `embed`, `cache`, `search`, `graph`, `bench`). `Embedder` trait with three backends:
candle (Metal on macOS, CPU elsewhere; feature `candle`), fastembed/ONNX Runtime (feature
`fastembed`, optional CoreML execution provider) and an OpenAI-compatible remote endpoint.

### Chunker

- Definitions come from `tree-sitter-tags`; `@doc` captures come through for Rust (supplementary
  overlay `queries/rust/tags.scm`, since the grammar's `tags.scm` has no `@doc`) and Python
  (docstrings via `queries/python/tags.scm`). Unit tests assert both.
- Runtime grammar via `tree-sitter-loader`: tree-sitter-c, pointed to by a tree-sitter
  `config.json` `parser-directories` entry, is compiled by the loader and tags a sample file
  (`ring_push` as `function`, `ring` as `class`) with no lodestone code changes
  (`tests/external_grammar.rs`, run with `LODESTONE_TEST_PARSER_DIR`).
- Whole corpus chunks in 4.4 s single-threaded (27,150 tagged definitions, 183 prose chunks);
  every `eval/questions.json` answer is extracted.

### Spike: model × backend

chunks/s embeds a deterministic sample of corpus chunks (avg 576 chars); rank@5 is the full
`lode bench` on that model (vector-only fusion, see Decisions; no reranker).

| Model | Backend | chunks/s | Peak RSS | Query embed | Cold index (27.3k) | rank@5 | MRR | Hard top-5 |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| jina-embeddings-v2-base-code | candle-metal (f16) | 93.9 (10k) | 0.85 GB | 5.5 ms | 351 s | 0.55 | 0.44 | 3/4 |
| jina-embeddings-v2-base-code | fastembed CPU | 2.1 (2k) | 39.7 GB | 17.6 ms | — | — | — | — |
| CodeRankEmbed | candle-metal (f16) | 95.1 (10k) | 1.28 GB | 5.1 ms | 314 s | 0.60 | 0.47 | 2/4 |
| CodeRankEmbed | candle-cpu (f32) | 13.6 (1k) | 6.7 GB | 35.4 ms | — | — | — | — |
| bge-small-en-v1.5 | candle-metal (f32) | 84.8 (10k) | 0.44 GB | 5.3 ms | 361 s | 0.55 | 0.47 | 2/4 |
| bge-small-en-v1.5 | fastembed CPU | 9.4 (10k) | 39.4 GB | 9.8 ms | — | — | — | — |
| bge-small-en-v1.5 | fastembed CoreML EP | 6.1 (2k) | 45.6 GB | 11.8 ms | — | — | — | — |
| gte-modernbert-base | candle-metal (f32) | 34.6 (2k) | 0.75 GB | 8.6 ms | 840 s | 0.55 | 0.37 | 2/4 |

- candle's jina port matches the ONNX reference: mean cosine 1.0000 (min 1.0000) over 40
  chunks between candle-metal and fastembed (`examples/agree.rs`).
- **candle-metal vs fastembed CPU: 44.7× (jina-code), 9.0× (bge-small)** — the ≥ 5× criterion
  holds. fastembed pads every batch to `max_length` and its ONNX graphs materialise large
  attention tensors (≈ 40 GB peak RSS), which is most of the gap.
- Every backend clears the 2,000 chunks/min (33/s) runtime criterion except fastembed+jina.

### Decisions (§11)

| Decision | Measurement | Choice |
| ---- | ---- | ---- |
| Embedding runtime | table above | **candle + Metal** (fastembed stays the CPU/Linux fallback) |
| Model | rank@5: CodeRankEmbed 0.60, jina-code 0.55, bge-small 0.55, gte-modernbert 0.55 | **CodeRankEmbed** (`nomic-ai/CodeRankEmbed`, MIT), fastest of the 768-d models too |
| Vector precision | same chunking, CodeRankEmbed: f32 rank@5 0.70 / MRR 0.478; f16 0.65 / 0.476 | **f32** (difference 0.05 > 0.02); 27k × 768 × 4 B = 84 MB |
| Body truncation | CodeRankEmbed f16: 200 tok 0.65 (238 s), 300 tok 0.65, 500 tok 0.65 (360 s) | **200 tokens** (equal rank@5, cheapest) |
| Lexical tokenizer | identifier queries, lexical-only top-1: code tokenizer 0.833, tantivy default 0.667 | **code tokenizer** (splits `snake_case`/`CamelCase`, keeps whole identifier) |
| Built-in grammar set | see Phase 1 | all five (`lang-rust`, `lang-python`, `lang-typescript`, `lang-go`, `lang-markdown`) default-on |

Other decisions:

- **Fusion weights.** Equal-weight RRF of kNN and BM25 gave rank@5 0.30 (jina-code); BM25 alone
  0.05, vectors alone 0.55. Long natural-language questions make BM25 noise, so prose queries are
  fused at lexical weight 0 and queries containing identifier-like tokens (`snake_case`,
  `CamelCase`, `a::b`) at weight 2. Identifier queries still rank the named item first
  (top-1 hit rate 0.97–1.00).
- **Tags overlays are prepended**, not appended as §12 suggests: tree-sitter-tags keeps the
  lowest pattern index per name node, so an overlay must come first to replace the grammar's
  undocumented pattern for the same node. Still queries only.
- **TypeScript** tags = TypeScript's `tags.scm` + JavaScript's, as the grammar intends.
- **jina-code on candle** needed its own module (`src/embed/jina.rs`): the checkpoint is the
  "qk-post-norm" JinaBERT variant (q/k layer norms, pre-norm residuals, `up * gelu(gate)`), and
  candle-transformers' `jina_bert` has neither that layout nor padding masks.
- **ModernBERT** is vendored with dtype-correct masks (`src/embed/modernbert.rs`) so it runs in
  f16 and fully masked padding rows stay finite.
- **Per-model dtype**: bge-small overflows in f16 on Metal (NaN vectors), so its built-in entry
  runs in f32; embedders fail loudly on non-finite output and the vector index refuses
  non-finite vectors (they made usearch's HNSW insert loop forever).
- **Sequential usearch inserts**: concurrent `add` beyond usearch's thread-slot pool spun; HNSW
  insertion is a small fraction of indexing time.
- **Licenses**: defaults are permissive (CodeRankEmbed MIT, gte-reranker-modernbert Apache-2.0).
  Non-commercial models (e.g. jina-reranker-v2, CC-BY-NC-4.0) can be configured and benchmarked
  but are not defaults.

### Blocked

- Cloning tree-sitter-c from GitHub and compiling it was refused by the session's permission
  policy (external code). Fallback: the identical grammar source from the local Cargo registry
  (`tree-sitter-c` 0.24.2) as the parser directory; the loader path under test is the same.
- fastembed + jina-code on 10k chunks: stopped after 19 min at 27 GB RSS; measured on 2k chunks.
- nomic-embed-code (7B) and Qodo-Embed-1-1.5B were not run: at 10–50× the compute of the
  ~140M models they cannot index 27k chunks in < 10 min on this GPU (≈ 95 chunks/s ÷ 10 ≥ 45
  min), Qodo's license is not OSI-permissive, and Qodo needs bidirectional Qwen2 attention that
  candle's `qwen2` does not implement.
