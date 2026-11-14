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
  (`tests/it/external_grammar.rs`, run with `LODESTONE_TEST_PARSER_DIR`).
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
| Built-in grammar set | release binary 66.7 MB with `lang-rust` only vs 71.1 MB with all five (+4.4 MB, +6.6%); +13 s incremental build | all five (`lang-rust`, `lang-python`, `lang-typescript`, `lang-go`, `lang-markdown`) default-on |

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

## Phase 1 — Index and search, in-process

Chunker with the five built-in grammars + loader, global embedding cache, per-worktree index
(manifest, `items.sqlite`, usearch, tantivy, `graph.json`), Cargo + fallback project-graph
providers, indexing loop, hybrid search, the §5.4 output format, and CLI `index`, `search`,
`status`, `bench`, `languages`, `models`.

Measured with the final defaults (CodeRankEmbed, candle-metal, f32 vectors, 200-token chunks)
on a fresh data directory:

| Criterion | Target | Measured | Result |
| ---- | ---- | ---- | ---- |
| Cold index of stacks-core on Metal | < 10 min | 245 s (27,418 items, all embedded) | pass |
| Re-index with no changes | < 5 s | 0.02 s | pass |
| Mixed-language repo, every extension accounted for in `status` | — | stacks-core: `.rs` 25,603 tagged, `.ts` 814 tagged + 252 text, `.md` 707, `.py` 23, `.js` 1 + 4 text, `.txt` 5 text; 26 other extensions listed as `skipped_files`; fixture test covers rs/ts/py/md/txt/toml/json | pass |
| Second worktree of the same commit | < 30 s | 18.6 s (0 embedded, 27,418 cached) | pass |
| `lode search` p50 warm, in-process | < 300 ms | 8.7 ms without reranker; 948 ms with the default reranker | pass without rerank (see Decisions) |
| rank@5 on `easy` + `mismatch` | ≥ 0.85 | 0.65 without reranker; 0.75 with reranker (MRR 0.489 / 0.585) | **fail** |
| All four hard helpers in top 5 | 4/4 | 2/4 without reranker; **4/4 with reranker** | pass with reranker |

Per-question ranks with the reranker: q01–q06 1,1,1,1,1,1; q07 4; q08 3; q09 1; q10 3; q11 1;
q12 9; q13 –; q14 –; q15 1; q16 –; q17 –; q18 3; q19 1; q20 3. Negative questions: the top
cosine similarity of negatives (max 0.385) overlaps positives (min 0.323), so no threshold
separates them with this model.

### Decisions

- **Reranker on by default** (`Alibaba-NLP/gte-reranker-modernbert-base`, Apache-2.0, top 50,
  192-token pairs, f16 on Metal): +0.10 rank@5, hard cases 2/4 → 4/4, at ~0.95 s per query. The
  user ruled that ~1 s is acceptable if it saves agent tool calls; Phase 4 measures that with
  rerank-on and rerank-off arms. Searches remain ~9 ms with `[rerank] enabled = false`.
- Test detection (`queries/rust/tests.scm`) covers `#[test]`, `#[tokio::test]`, `#[rstest]`,
  `#[apply(..)]`, `#[test_case]`, attributes separated by comments, and `#[cfg(test)]` items;
  doc comments placed after attributes are captured.
- Files whose grammar finds no definitions are chunked as prose rather than dropped.
- Item ids are never reused; the manifest is written after each 128-file batch, after items,
  lexical and vectors, so a crash leaves files either fully indexed or re-indexed on the next run.

### Blocked / not met

- **rank@5 ≥ 0.85 is not met** (best 0.75). Tried: five embedding models (table in Phase 0),
  vector/lexical fusion weights, sentence-level query vectors (0.55–0.60, no gain), chunk sizes
  200/300/500 (no difference), and three cross-encoders (bge-reranker-base 0.45; jina-reranker-v2
  via fastembed did not finish 78 queries in 25 min on CPU; gte-reranker-modernbert 0.75). The four
  questions no configuration retrieves (q13, q14, q16, q17) describe domain behaviour in words
  that share nothing with the code or its docs.

## Phase 2 — Daemon, watcher, MCP

`lode serve`: registry (the only place paths are canonicalized), `ArcSwap` snapshots with
copy-on-write vector index, one async writer lock per worktree, notify watcher (500 ms
debounce, `.git/HEAD`/refs → full rescan, `.gitignore`/`.lodestoneignore`/`CACHEDIR.TAG`
filtering, 60 s poll fallback, `gitdir:` worktrees), REST (§6.2), MCP streamable HTTP with
exactly two tools (§6.1), and CLI fallback to HTTP when a daemon answers.

Real-model measurements (CodeRankEmbed on Metal, daemon on a second worktree of stacks-core);
the same scenarios also run as `tests/it/daemon.rs` with a deterministic embedder.

| Criterion | Target | Measured | Result |
| ---- | ---- | ---- | ---- |
| 8 clients × 200 searches with watcher reindex underneath | 0 errors, 0 stale-read panics | 1,600 requests, 0 errors, 34 file edits reindexed during the run | pass |
| … latency, reranker off | p50 < 250 ms, p99 < 1 s | p50 40.1 ms, p99 43.2 ms | pass |
| … latency, reranker on (default) | p50 < 250 ms, p99 < 1 s | 8 × 20: p50 7.6 s, p99 7.9 s (cross-encoder serialized on the GPU), 0 errors | **fail** (see Decisions) |
| Edit a file → item searchable | < 2 s | 0.69–0.72 s (reranker off); 1.96 s (reranker on, polling at ~1 s/search) | pass |
| Branch switch → fully fresh | < 30 s, ≥ 95% cache hits | to a commit 15 back (58 files): 5.4 s, 95.4% hits; back: 3.8 s, 100% | pass |
| Kill daemon mid-index, restart | consistent index | killed during the first batch; restart reported stale with the reason, reused 5,388 cached vectors, final manifest ids = items ids (27,418), vectors 27,418 | pass |
| Claude Code via `.mcp.json` | two tools, exact descriptions | `claude -p` init: lodestone `connected`, tools `mcp__lodestone__search` and `mcp__lodestone__status`; descriptions echoed verbatim | pass |

### Decisions

- **Reranker throughput under concurrency.** The cross-encoder holds a mutex and saturates the
  GPU, so concurrent searches queue. Kept on by default per the user's call (agent sessions
  issue searches serially); multi-client deployments can set `[rerank] enabled = false` for
  ~40 ms p50.
- Searches during indexing are answered from the previous snapshot and flagged
  `stale: true` with `indexing in progress` (no blocking, no half-built state).
- The watcher only consults root-level ignore files; a later full rescan (branch switch,
  daemon start) reconciles anything nested ignore rules would exclude.
- `register` returns immediately and indexes on the worktree's worker; `index` over REST waits
  for completion (what `lode index` needs).

## Phase 3 — Quality (gated)

Each change was measured against the Phase 1 configuration on `eval/questions.json` and on a
second repository (`eval/questions-tantivy.json`: 14 questions against the tantivy 0.27.0 crate
source, 8,724 items, cold index 69.8 s) before trusting it. Gate: keep only if rank@5 or the
hard-case hit rate rises; p50 ≤ 400 ms with rerank on (overridden by the user, see below).

| Change | stacks-core rank@5 / MRR / hard | tantivy rank@5 / MRR | p50 | Kept |
| ---- | ---- | ---- | ---- | ---- |
| Baseline (dense only, no rerank) | 0.65 / 0.489 / 2/4 | 0.583 / 0.452 | 8.7 ms | — |
| Reranker (gte-reranker-modernbert, top 50, 192 tok) | **0.75 / 0.585 / 4/4** | **0.667 / 0.601** | 948–1,025 ms | **yes** (user override of the 400 ms gate) |
| Identifier boosting (exact name first + BM25 ×2 for identifier queries), on top of rerank | 0.75 / 0.585 / 4/4 vs 0.75 / 0.618 / 4/4 without | 0.667 / 0.601 either way | — | no (no gain; identifier hit rate 0.967 either way) |
| Test down-weighting (`include_tests` + `test_weight` 0.5 / 0.8 / 1.0) vs filtering tests | 0.70 / 0.537–0.568 / 3/4 vs 0.75 / 0.585 / 4/4 | — | — | no (filtering stays the default) |
| `module` chunks (one file-level item per tagged file) | 0.75 / 0.585 / 4/4 (797 extra items) | 0.667 / 0.588 | — | no |
| Sentence-level query vectors (Phase 1 trial) | 0.55–0.60 without rerank | — | +6 ms | no |

Other cross-encoders tried: `BAAI/bge-reranker-base` (MIT) lowered rank@5 to 0.45 at 1.3 s;
`jina-reranker-v2-base-multilingual` (CC-BY-NC-4.0, fastembed CPU) did not finish 78 queries in
25 minutes. Reranker token budget: 128 tok → 0.65 / 679 ms, 192 tok → 0.75 / 948 ms (f16),
256 tok → 0.75 / 1,790 ms (f32), 512 tok → 0.70 / 5,426 ms (f32).

### Decisions

- **Reranker kept** despite p50 ≈ 0.95 s: the user judged ~1 s acceptable if it saves agent tool
  calls; Phase 4 runs rerank-on and rerank-off arms to check that.
- **Identifier boosting off by default** (`exact_identifier_first = false`,
  `identifier_lexical_weight = 0`): it raised nothing on either set. The BM25 index is still
  built and can be re-enabled per config; search is therefore dense + rerank by default.
- **Index-time settings are fingerprinted**: chunk budget, module/summary chunking and every
  language's query texts hash into `manifest.chunker`; a mismatch re-chunks all files on the next
  full index (embeddings for unchanged text stay cached). Found because the first module-chunk
  run silently re-used old chunks.
- **ModernBERT reranker on candle**: candle's classification head ends in a softmax (constant
  for one logit), so the head is applied in lodestone; the encoder is the vendored f16 ModernBERT.

### Not met

- p50 ≤ 400 ms with rerank on (measured ~950 ms; the cross-encoder runs at ≈ 3 TFLOPS effective
  in candle's Metal kernels, sub-batching does not help). Accepted by the user's override.
- rank@5 ≥ 0.85 remains unmet (0.75).

## Phase 4 — End-to-end agent eval

`eval/harness/run_trials.py` gained a `rule_lode` arm: the shared reuse rule plus
`lode-must.md` (mandatory `search` call), `--mcp-config` pointing at the daemon, only the two
lodestone tools allowed besides Read/Grep/Glob. `eval/harness/setup_scratch.sh` builds the
scratch layout. 18 tasks × 3 reps on Sonnet, 4 jobs in parallel, daemon serving stacks-core
`95c6a45eed` with the Phase 3 defaults. Two `rule_lode` arms (reranker on, the default, and off)
plus a fresh `rule` arm for a same-day baseline (the pilot numbers are from a different day).

| Arm | Reuse | Hard cases (tasks with ≥ 1 reuse; runs) | Median cost/run | Median tokens | Tool calls/run (search + Grep + Read + Glob) | Median turns | Median wall |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| pilot: stock | 23/54 | 0/4 | $0.031 | — | — | — | — |
| pilot: reuse rule | 38/54 | 0/4 | $0.039 | — | — | — | — |
| pilot: rule + compact catalogue in prompt | 46/54 | 3/4 | $0.042 | — | — | — | — |
| pilot: rule + reuse skill | 42/54 | 3/4 | $0.156 | — | — | — | — |
| pilot: rule + `ck` forced | n/a | 1/4 | $0.064 | — | — | — | — |
| today: reuse rule (no lodestone) | 40/54 | 2/4; 2/12 | $0.041 | 36.4k | 4.61 (0 + 3.43 + 1.13 + 0.06) | 5.5 | 17 s |
| **rule_lode, reranker on (default)** | **44/54** | **2/4; 5/12** | **$0.038** | 40.5k | 5.15 (1.13 + 2.72 + 1.30) | 6 | 16 s |
| rule_lode, reranker off | 42/54 | 2/4; 4/12 | $0.033 | 34.2k | 4.85 (1.07 + 2.41 + 1.37) | 5 | 15 s |

Total spend: $2.24 (rerank on), $1.99 (rerank off), $2.39 (baseline rule).

| Criterion | Target | Measured | Result |
| ---- | ---- | ---- | ---- |
| Hard cases | ≥ 3/4 | 2/4 (t07 bounded_format 3/3, t18 select_transactions_where 2/3) | **fail** |
| Cost | ≤ 1.3 × rule_cat_prompt ($0.055) | $0.038 median (0.90×); $0.033 with rerank off (0.79×) | pass |

### Findings

- Reuse rises from 40/54 (same-day rule baseline) to 44/54 with lodestone, at lower median
  cost; t07 goes from 1/3 to 3/3, t10 from 2/3 to 3/3, t18 from 1/3 to 2/3.
- **t08 (guarded_string) is a retrieval hit, not a miss**: `search` returned `guarded_string!`
  as the top result in 3/3 runs; the agent read it and declined it because stacks-common has
  no `regex` dependency. The grader counts that as no reuse.
- **t06 (RetryReader) and t09 (check_codec_and_corruption) fail on query framing**: agents
  searched for the task's own nouns ("Either enum, decode one of two types", "NeighborAddress
  consensus serialization") rather than the capability they would need (rewinding a reader, a
  codec round-trip test helper). No retrieval change reaches those; the compact catalogue in
  the prompt does, because the agent does not have to know what to ask.
- **Reranker vs agent tool calls (the user's 1 s criterion)**: with the reranker, agents made
  slightly *more* tool calls (5.15 vs 4.85 per run), used more tokens (40.5k vs 34.2k median)
  and cost more ($0.038 vs $0.033), for +2/54 reuse and +1/12 hard-case runs — within run-to-run
  noise at n = 54. Searches replace Grep (−0.7 to −1.0 Grep per run vs the rule baseline) but
  do not reduce total calls. On this evidence the reranker does not pay for itself in agent
  work; it remains the default only because that was the user's call, and turning it off is a
  one-line config change (`[rerank] enabled = false`).

## Experiment — Doc-comment vectors

`doc_vectors = true` embeds each documented item's `<kind> <qualified name>\n<doc>` as a
second vector in `doc-vectors.usearch`, keyed by item id and cached like code vectors. Search
fuses the doc list into RRF at `[search] doc_weight`, but only while the reranker is on.
Turning the setting on for an existing index fills the doc vectors on the next index (code
vectors come from the cache). Zero-weighted fusion lists now add no candidates, which also
skips the unused BM25 query on every search.

Measured on one index per corpus, so the baseline is the same index with `doc_weight = 0`:

| Corpus | Reranker | Fusion | rank@5 | MRR | Identifier top-1 |
| ---- | ---- | ---- | ---- | ---- | ---- |
| stacks-core | off | none (baseline) | 0.700 | 0.538 | 0.833 |
| stacks-core | off | sum, w 1.0 | 0.450 | 0.374 | 0.567 |
| stacks-core | off | sum, w 0.5 | 0.450 | 0.402 | 0.567 |
| stacks-core | off | best rank, w 1.0 | 0.650 | 0.520 | 0.667 |
| stacks-core | on | none (baseline) | 0.750 | 0.618 | 0.967 |
| **stacks-core** | **on** | **sum, w 1.0** | **0.800** | **0.643** | **1.000** |
| stacks-core | on | best rank, w 1.0 | 0.750 | 0.618 | 1.000 |
| tantivy | off | none (baseline) | 0.583 | 0.452 | 1.000 |
| tantivy | off | sum, w 1.0 | 0.583 | 0.572 | 0.917 |
| tantivy | off | best rank, w 1.0 | 0.583 | 0.467 | 1.000 |
| tantivy | on | none (baseline) | 0.667 | 0.601 | 1.000 |
| tantivy | on | sum, w 1.0 | 0.667 | 0.596 | 1.000 |
| tantivy | on | best rank, w 1.0 | 0.667 | 0.590 | 1.000 |

| Cost | stacks-core | tantivy |
| ---- | ---- | ---- |
| Documented items embedded | 8,929 of 27,418 (+33%) | 2,243 of 8,724 (+26%) |
| Enabling on a cached index | 62.5 s | 16.5 s |
| `doc-vectors.usearch` | 29 MB (vs 88 MB code) | 7 MB (vs 29 MB) |
| Search p50, reranker on | +33 ms (1,023 vs 989 ms) | +9 ms |

### Decisions

- **Summed fusion, applied only with the reranker.** Without the reranker, summing lets any
  documented item ranked mid-list in both lists beat an undocumented item that code ranks
  first (q05, q15, q19 fall from rank 1 to the 20s). With the reranker, the doc list instead
  widens its 50-candidate pool, and the reranker sorts it out. Best-rank fusion (each item
  scored by its better list) never hurt, but never helped either.
- **Off by default.** The gain is one question per corpus: stacks-core q17 goes from unranked
  to rank 2, and it is one of the four questions no earlier configuration retrieved. On
  tantivy, t06 goes from unranked to 16 while t12 drops out of the top 50, a net zero. That
  does not clear the Phase 3 bar for a default change, given +26–33% embeddings at index time.

## Bench — agent-view metric

Agents receive the top 8 results per search by default (`DEFAULT_TOP_K`, shared by MCP, REST,
the CLI and the bench), each as one line they can screen without opening the file. `lode bench`
now reports, as its first summary line, the share of answerable questions whose answer is
within that top 8. rank@1/5/10, MRR, per-kind rank@5, hard cases, the similarity gap and the
identifier hit rate are unchanged; hard cases are also reported within the top 8. Hard-case ids
that are not in the question file are ignored instead of counted as misses.

Recomputed from the doc-vector experiment's runs (same index per corpus):

| Corpus | Configuration | Top 8 | rank@5 | rank@10 | MRR |
| ---- | ---- | ---- | ---- | ---- | ---- |
| stacks-core | reranker off | 14/20 (0.700) | 0.700 | 0.700 | 0.538 |
| stacks-core | reranker on (default) | 15/20 (0.750) | 0.750 | 0.800 | 0.618 |
| stacks-core | reranker on + doc vectors | 17/20 (0.850) | 0.800 | 0.850 | 0.643 |
| tantivy | reranker off | 7/12 (0.583) | 0.583 | 0.583 | 0.452 |
| tantivy | reranker on (default) | 10/12 (0.833) | 0.667 | 0.833 | 0.601 |
| tantivy | reranker on + doc vectors | 9/12 (0.750) | 0.667 | 0.833 | 0.596 |

What an agent sees shifts the reading of the reranker: on tantivy it brings three answers into
view (t04 21→7, t05 29→1, t07 48→7), while on stacks-core most of its MRR gain reorders answers
already in the top 8 (net +1: q06 43→1, q12 3→9).

## Output — line ranges

Each result line now starts with `path:start-end`, the definition's inclusive line range
(preceding doc comments excluded), so an agent's second pass can read exactly the item instead
of guessing a window. JSON results gain `line_end` next to `line`. This deviates from PLAN.md
§5.4 and the verbatim §6.1 tool description at the user's request: the `search` description now
says `'path:start-end kind name(signature) — doc', where start-end is the item's line range`.

Range sizes on stacks-core (non-test items) show where reading the whole range is cheap and
where it is not:

| Kind | Items | Median lines | p95 lines | Max lines |
| ---- | ---- | ---- | ---- | ---- |
| method | 8,123 | 7 | 64 | 619 |
| function | 5,902 | 12 | 91 | 2,075 |
| impl | 2,445 | 18 | 391 | 6,439 |
| class | 1,634 | 6 | 45 | 555 |
| constant | 1,345 | 1 | 24 | 1,026 |

## Output — name position

Each result now starts with the name's position in compiler style, followed by the item's line
range: `path:line:col  lines start-end  signature  — doc`. JSON results gain `name_line` and
`name_column`. All numbers stay 1-based, matching grep, sed, git, compiler diagnostics, editors
and the Read tool; language servers are the 0-based exception, and the `search` tool description
now says the numbers are 1-based. Columns count characters (LSP counts UTF-16 units; they agree
unless non-ASCII text precedes the name). A hyphen after the column was avoided because
GNU-style tools read `line:col-n` as a column range.

The position comes from the tags query's name capture, so it is language-independent. Items
gained two columns: `items.sqlite` is migrated in place, and an item-format version in the
chunker fingerprint re-chunks existing indexes on the next full index, with embeddings from the
cache (tantivy: 351 files, 0 embedded, 6.3 s). Until then, results show the start line without a
column.

Checked against rust-analyzer on this repository: three positions from `lode search`, minus 1,
resolved on the first call with no file read (references to `VectorIndex::duplicate`, callers of
`WorktreeIndex::repair`, references to `format::line`); for `repair` the position equals
rust-analyzer's own selection range.

## Model comparison

Embedding models, rerankers, reranker pool size and doc vectors, measured on 137 answerable
questions (plus 21 with no answer) across three repositories: stacks-core (64), tantivy (39) and
LibreChat (34, TypeScript/JavaScript). "Top 8" is the share of questions whose answer is in the 8
results an agent receives; comparisons are paired per question with 95% bootstrap intervals
(`eval/compare.py`). Every model's scores were checked against its reference implementation
(transformers; Jina's own code pinned at `9cfeff2d` and `d7d7e73b` for its two rerankers) and
match to three decimals or within half-precision rounding. Apple M4 Max; latency is one search at
a time.

### Embedding models

| Model | Licence | Top 8, no reranker | Top 8, reranker (pool 50) | Recall@50 | Chunks/s, Metal | Chunks/s, CPU |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| **Qwen3-Embedding-0.6B** | Apache-2.0 | **0.825** | **0.861** | **0.949** | 42.7 | 6.6 |
| jina-code-embeddings-0.5b | CC-BY-NC-4.0 | 0.752 | 0.854 | 0.927 | 58.1 | 6.8 |
| harrier-oss-v1-0.6b | MIT | 0.781 | 0.825 | 0.927 | 32.2 | — |
| granite-embedding-english-r2 | Apache-2.0 | 0.723 | 0.818 | 0.891 | 57.8 | 10.9 |
| jina-embeddings-v2-base-code | Apache-2.0 | 0.715 | 0.810 | 0.869 | 107.1 | 14.1 |
| granite-embedding-small-english-r2 | Apache-2.0 | 0.679 | 0.796 | 0.839 | 134.9 | 27.9 |
| UltraBERT-Embed-150M | Apache-2.0 | 0.730 | 0.788 | 0.832 | 93.3 | 13.3 |
| CodeRankEmbed (previous default) | MIT | 0.701 | 0.781 | 0.861 | 106.2 | 16.8 |
| all-MiniLM-L6-v2 | Apache-2.0 | 0.628 | 0.774 | 0.818 | 205.8 | 78.3 |
| EmbeddingGemma 2 | Apache-2.0 + Gemma use policy | 0.672 | 0.708 | 0.788 | 43.9 (f32) | 10.5 |

Qwen3-Embedding-0.6B against CodeRankEmbed: +0.124 top 8 without a reranker (21 questions
gained, 4 lost, interval +0.058 to +0.197) and +0.080 with it. Its first index of stacks-core takes
657 s on Metal (264 s for CodeRankEmbed). The CPU column is candle on this machine's CPU, 500
chunks each; at 6.6 chunks/s a 27k-chunk repository needs about 70 minutes to index on CPU.

EmbeddingGemma 2 and UltraBERT-Embed-150M (pinned at `b06b3672`) needed their own encoders
(`src/embed/embedding_gemma.rs`, `src/embed/ultrabert.rs`); both match their transformers
reference. EmbeddingGemma 2 runs in f32, as its model card warns of NaNs in f16. UltraBERT
against CodeRankEmbed: +0.029 without a reranker and +0.044 [−0.029, +0.124] at pool 20, neither
significant, and 21% slower on CPU.

### Rerankers (all on CodeRankEmbed's 50 candidates)

| Reranker | Licence | Top 8 | MRR | Search p50 | vs default |
| ---- | ---- | ---- | ---- | ---- | ---- |
| jina-reranker-v3 | CC-BY-NC-4.0 | 0.839 | 0.736 | 5.9 s | +0.058 [+0.015, +0.102] |
| Qwen3-Reranker-0.6B | Apache-2.0 | 0.825 | 0.719 | 3.9 s | +0.044 [+0.000, +0.095] |
| **gte-reranker-modernbert-base** (default) | Apache-2.0 | 0.781 | 0.653 | 1.0 s | — |
| (no reranker) | — | 0.701 | 0.531 | 7 ms | −0.080 |
| jina-reranker-v2-base-multilingual | CC-BY-NC-4.0 | 0.642 | 0.439 | 0.5 s | −0.139 |
| bge-reranker-v2-m3 | Apache-2.0 | 0.613 | 0.422 | 2.0 s | −0.168 |
| granite-embedding-reranker-english-r2 | Apache-2.0 | 0.606 | 0.429 | 1.1 s | −0.175 |
| ms-marco-MiniLM-L6-v2 | Apache-2.0 | 0.496 | 0.360 | 0.3 s | −0.285 |

Four of the seven rank code worse than no reranker at all; their reference implementations score
the same, so this is how they behave on code, not a porting error.

### Reranker pool size (default reranker)

| Pool | Qwen3-Embedding-0.6B top 8 | p50 | CodeRankEmbed top 8 | p50 |
| ---- | ---- | ---- | ---- | ---- |
| 15 | 0.847 | 306 ms | 0.745 | 281 ms |
| **20** | **0.869** | **429 ms** | 0.737 | 376 ms |
| 25 | 0.861 | 511 ms | 0.745 | 479 ms |
| 50 | 0.861 | 998 ms | 0.781 | 986 ms |
| 75 | 0.847 | 1,459 ms | 0.788 | 1,450 ms |
| 100 | 0.847 | 2,026 ms | 0.796 | 2,082 ms |

With Qwen3 the pool can shrink to 20 at no cost (+0.007 [−0.029, +0.044] against 50); beyond 50
it gets slightly worse, as the reranker meets more near-misses. CodeRankEmbed needs the larger
pool because its answers sit deeper in the candidate list.

### Doc vectors (reranker on, pool 50)

| Model | Top 8 without | Top 8 with | Change |
| ---- | ---- | ---- | ---- |
| Qwen3-Embedding-0.6B | 0.861 | 0.832 | −0.029 [−0.058, −0.007] |
| granite-embedding-english-r2 | 0.818 | 0.832 | +0.015 |
| CodeRankEmbed | 0.781 | 0.825 | +0.044 [+0.000, +0.095] |

### Decisions

- **Default embedding model: Qwen3-Embedding-0.6B on Apple GPUs, CodeRankEmbed on CPU.** Qwen3
  is the best model measured, significantly, with and without a reranker; its slower first index
  (about 11 minutes for stacks-core) runs in the background and later edits embed only what
  changed. On CPU it would take about 70 minutes for the same repository, so CPU builds keep
  CodeRankEmbed (about 27 minutes).
- **Default CPU backend: candle on the CPU, not fastembed.** The previous non-macOS default paired
  CodeRankEmbed with fastembed, which has no build of it, so it failed to load; candle runs every
  built-in model, and fastembed stays available for the models it ships.
- **Reranker stays gte-reranker-modernbert-base, pool reduced from 50 to 20.** Same quality with
  the new embedder, search p50 from about 1 s to about 0.4 s, and proportionally shorter waits when
  several agents search at once. Qwen3-Reranker-0.6B and jina-reranker-v3 rank better but cost
  four to six times the latency, and jina-reranker-v3 is non-commercial; both can be configured.
- **Doc vectors stay off.** They help CodeRankEmbed but hurt the new default.
- **CPU default stays CodeRankEmbed.** UltraBERT-Embed-150M is not significantly better and is
  slower on CPU (13.3 against 16.8 chunks/s); EmbeddingGemma 2 is worse on both counts. Both
  are built in as `ultrabert-150m` and `embeddinggemma-2`.
- Upgrading: an index built with CodeRankEmbed is reported stale against the new default until
  `lode index --force` re-embeds it.

### Not covered

- **BGE-M3** ships no safetensors weights, only a PyTorch pickle, which lodestone does not load.
- **snowflake-arctic-embed-m-v2.0** needs model code candle does not have; not attempted.
- **8B models and Nemotron-Embed 1B** (OpenMDW licence) are far beyond the indexing-speed budget.
- **Hosted embedding APIs** (Voyage, OpenAI, Gemini) were not measured: they need keys and cost
  per index.
- **Linux CPU numbers** come from this Mac's CPU; a run on Linux runners was proposed, not done.
- The question writers read the code's doc comments, which may favour documented items slightly.

## Settings sweep

Query phrasing, reranker × embedder pairs, chunk size and search settings, on the same 137
questions with paired 95% bootstrap intervals. Without a reranker, or with the default reranker at
pool 20; the baselines reproduce the model comparison exactly. Half of the runs were on an M4 Pro,
which gave per-question ranks identical to the M4 Max; all times below are from the M4 Max. With
about 50 comparisons, one or two narrowly significant results are expected by chance.

### Query phrasing

Each question was rewritten in four styles by writers who saw only the question, not the code
(`eval/variants/`, built with `build.py`): **terse** keywords, a **short** question, a **task**
("find the code that…") and **identifier** (guessed function or type names in code vocabulary).
`build.py` derives three more: **combined** (the task sentence, then the identifier guesses on a
new line) and, as a worst case for guessing, **identifier** and **combined** with every word that
contains an answer symbol removed ("names removed").

| Style (top 8, no reranker / reranker) | CodeRankEmbed | Qwen3-Embedding-0.6B | granite-small-r2 |
| ---- | ---- | ---- | ---- |
| original question | 0.701 / 0.737 | 0.825 / 0.869 | 0.679 / 0.745 |
| terse | 0.832 / 0.876 | 0.847 / 0.876 | 0.825 / 0.847 |
| short | 0.854 / 0.869 | 0.869 / 0.898 | 0.766 / 0.847 |
| task | 0.883 / 0.891 | 0.942 / 0.934 | 0.869 / 0.898 |
| identifier | **0.949 / 0.978** | **0.949 / 0.978** | **0.920 / 0.971** |

Identifier rewrites happen to name an answer symbol in 82 of 137 questions, but the gain holds on
the other 55: +0.20 for CodeRankEmbed and +0.15 for Qwen3 without a reranker. Guessed names help
even when wrong. With identifier-style queries the three embedders score within 0.03 of each
other. Terse queries help the small models but not Qwen3 (+0.02, not significant).

Task, identifier and combined queries on six embedders (top 8, no reranker / reranker / reranker
at 384 tokens; pool 20):

| Embedder | Original | Task | Identifier | Combined | Combined, names removed |
| ---- | ---- | ---- | ---- | ---- | ---- |
| Qwen3-Embedding-0.6B | **0.825 / 0.869** | **0.942 / 0.934** | 0.949 / 0.978 | 0.978 / 0.971 / **0.985** | **0.956** / 0.934 / 0.949 |
| jina-embeddings-v2-base-code | 0.715 / 0.759 | 0.869 / 0.898 | 0.927 / 0.978 | **0.985** / 0.971 / **0.985** | **0.956 / 0.942 / 0.956** |
| UltraBERT-Embed-150M | 0.730 / 0.781 | 0.854 / 0.891 | **0.971 / 0.985** | 0.971 / 0.956 / 0.978 | 0.927 / 0.927 / 0.949 |
| CodeRankEmbed | 0.701 / 0.737 | 0.883 / 0.891 | 0.949 / 0.978 | 0.956 / 0.964 / 0.978 | 0.934 / 0.912 / 0.927 |
| granite-embedding-english-r2 | 0.723 / 0.781 | 0.912 / 0.927 | 0.956 / 0.978 | 0.956 / 0.956 / 0.971 | 0.942 / 0.927 / 0.942 |
| granite-small-r2 | 0.679 / 0.745 | 0.869 / 0.898 | 0.920 / 0.971 | 0.942 / 0.956 / 0.971 | 0.920 / 0.934 / 0.934 |

- **Qwen3 leads only on the original wording**, by 0.095 to 0.146 over every other model (all
  significant). With combined queries no model's interval against it excludes zero in any mode,
  with or without the answer names; jina-embeddings-v2-base-code ties or leads it everywhere (+0.007 at
  most) at 2.5× its Metal indexing speed. The benchmark is near its ceiling here: the spread
  between models is one to six questions.
- **Combined is the most robust style.** Without a reranker it beats identifier for Qwen3 (+0.029
  [+0.007, +0.058]) and jina-code-v2 (+0.058 [+0.022, +0.102]) and ties it for the rest. Removing
  the answer names costs identifier queries 0.05 to 0.11 but combined queries only 0.01 to 0.05:
  the task sentence carries the query when the guesses are wrong.
- **The reranker adds little to combined queries.** The query uses part of its 192-token budget,
  leaving less of each candidate: top 8 moves by −0.015 to +0.015 (none significant). At 384
  tokens it adds 0 to +0.029 over no reranker, at about 1.9× the search latency of the default
  (p50 about 735 ms against 380 ms).
- **These rewrites partly recall the repositories.** Their writers were told each repository's
  name, and tantivy and LibreChat are public; most of their identifier guesses match real names
  (30 of 34 for LibreChat). Identifier and combined scores here therefore overstate unfamiliar
  code; see the hard question set below.

### Reranker × embedder (pool 20)

| Embedder (top 8 / search p50) | gte-reranker-modernbert-base (default) | Qwen3-Reranker-0.6B | jina-reranker-v3 |
| ---- | ---- | ---- | ---- |
| Qwen3-Embedding-0.6B | 0.869 / 429 ms | **0.920** / 1,115 ms | 0.905 / 1,054 ms |
| granite-small-r2 | 0.745 / 357 ms | 0.766 / 1,104 ms | 0.766 / 1,202 ms |
| CodeRankEmbed | 0.737 / 376 ms | 0.752 / 1,103 ms | 0.745 / 1,201 ms |

The order is the same for every embedder; there is no pairing effect. Qwen3-Reranker on Qwen3's
candidates gains +0.051 [+0.022, +0.088] (7 questions gained, none lost) for about 2.6× the
search latency.

### Chunk size (`chunk_tokens`)

| Model (top 8, no reranker / reranker) | 128 | **200** | 350 | 500 |
| ---- | ---- | ---- | ---- | ---- |
| CodeRankEmbed | 0.693 / 0.759 | 0.701 / 0.737 | 0.745 / 0.766 | 0.737 / 0.759 |
| Qwen3-Embedding-0.6B | 0.803 / 0.832 | 0.825 / 0.869 | 0.847 / 0.861 | 0.847 / 0.861 |
| granite-small-r2 | 0.642 / 0.730 | 0.679 / 0.745 | 0.686 / 0.759 | 0.701 / 0.745 |

At 500 and above, the model's `max_tokens` was raised so that chunks are not truncated a second
time; Qwen3 at 1,000 scores 0.847 / 0.869. Only CodeRankEmbed gains significantly: at 350, +0.044
[+0.007, +0.088] without a reranker and +0.029 [+0.007, +0.058] with it. Chunks are truncated,
not split, so larger chunks cost more per definition:

| Indexing time relative to 200 | 128 | 350 | 500 |
| ---- | ---- | ---- | ---- |
| Qwen3-Embedding-0.6B, Metal | 0.74× | 1.31× | 1.59× (2.18× at 1,000) |
| granite-small-r2, Metal | 0.72× | 1.46× | 1.86× |
| CodeRankEmbed, CPU | 0.89× | 1.29× | 1.50× |
| granite-small-r2, CPU | 0.81× | 1.26× | 1.52× |

Raising the reranker's `max_tokens` from 192 to match larger chunks adds +0.008 to +0.029 against
the same chunk size at 192 (significant only for Qwen3 at 500 and 1,000 tokens with 512). The best
such setup, Qwen3 at 1,000 tokens with 512, reaches 0.898 against the default's 0.869 (+0.029
[−0.007, +0.073]) at 2.2× the indexing time and 3.6× the search latency.

### Search settings

- **`lexical_weight`** (fusion with full-text search): 0.25 is neutral; 0.5 and 1 hurt, most
  without a reranker (−0.21 top 8 for CodeRankEmbed at 1). With the reranker, only CodeRankEmbed
  at 0.5 breaks even.
- **`identifier_lexical_weight`**, on identifier-style queries: 1 and 2 change top 8 by at most
  ±0.015 and lower MRR; adding `exact_identifier_first` costs 0.03–0.05 top 8.
- **Qwen3 query instruction:** the default ("Given a description of what some code should do…") is
  the best of those tried. A generic code-snippet instruction scores −0.007, an agent-oriented one
  −0.029, no instruction −0.109 and Qwen's web-search instruction −0.219 (no reranker).

### Decisions

- **Defaults unchanged.** CodeRankEmbed at 350 tokens is the only significant chunk-size gain, it
  is narrow, and it costs 29% more CPU indexing time; Qwen3 and granite gain nothing significant.
  Qwen3-Reranker stays opt-in for its latency. Fusion, identifier weighting and the Qwen3
  instruction all stay as they are.
- **Query phrasing is the largest lever measured**, larger than any model or setting change. The
  agent skill and the search tool's description should ask for combined queries: one sentence on
  what the code does, then guessed function or type names; a wrong guess still helps.
- **Default embedder stays Qwen3-Embedding-0.6B for now** (superseded by the hard question set). With combined queries
  jina-embeddings-v2-base-code matches it at a fraction of the indexing cost, but Qwen3 is about
  0.11 ahead on poorly phrased queries, and this question set cannot separate the leaders. The
  hard question set below can.
- **Reranker stays on at 192 tokens** (superseded by the hard question set). It barely changes combined queries, but it adds 0.036 to
  0.066 on the original wording, which is what a poorly phrased query looks like.

## Hard question set

96 harder questions, 32 per repository (`eval/questions-hard-*.json`, method in `eval/README.md`):
answers without doc comments, behaviour past the embedded part of long definitions, look-alike
definitions and a vocabulary gap, chosen with grep only and re-checked by a separate reviewer.
Combined queries were rewritten from the question text by writers told to invent every name. All
runs without a reranker.

| Embedder (top 8) | Original question | Combined | Combined, names removed | Combined, recalled names | vs Qwen3, combined |
| ---- | ---- | ---- | ---- | ---- | ---- |
| **jina-embeddings-v2-base-code** | **0.646** | **0.948** | **0.948** | **0.969** | +0.094 [+0.042, +0.156] |
| Qwen3-Embedding-0.6B | 0.615 | 0.854 | 0.854 | 0.948 | — |
| CodeRankEmbed | 0.542 | 0.844 | 0.844 | 0.948 | −0.010 [−0.073, +0.052] |
| granite-embedding-english-r2 | 0.479 | 0.750 | 0.729 | 0.927 | −0.104 [−0.188, −0.021] |
| UltraBERT-Embed-150M | 0.427 | 0.708 | 0.698 | 0.896 | −0.146 [−0.250, −0.042] |
| granite-small-r2 | 0.396 | 0.698 | 0.698 | 0.896 | −0.156 [−0.250, −0.073] |

- **jina-embeddings-v2-base-code is the most accurate model measured.** With combined queries it
  finds 9 answers Qwen3 misses and misses none Qwen3 finds, in all three repositories and under
  every criterion. Its MRR is the same as Qwen3's (0.697 against 0.688): it gets more answers into
  the top 8 without ranking them higher.
- **Qwen3's lead on poorly phrased queries does not hold here.** On the original wording jina-code-v2
  scores +0.031 [−0.062, +0.125] against it and CodeRankEmbed −0.073 [−0.167, +0.031]; granite-r2,
  UltraBERT and granite-small are significantly worse.
- **Recalled names hide the differences.** Rewriters told the repository's name named an answer in
  69 of 96 questions (5 of 96 without), and with those queries every model scores 0.90 to 0.97.
- **Behaviour past the embedded text is the hardest case** (40 questions): 0.88 for jina-code-v2,
  0.78 for Qwen3, 0.72 for CodeRankEmbed and 0.60 for granite-r2.

### Reranker on the hard set (pool 20)

| Top 8 (search p50) | jina-code-v2, no reranker | jina-code-v2, reranker | Qwen3, no reranker | Qwen3, reranker |
| ---- | ---- | ---- | ---- | ---- |
| Combined | **0.948** (9 ms) | 0.833 (364 ms) | 0.854 (21 ms) | 0.760 (381 ms) |
| Original question | 0.646 | 0.698 | 0.615 | 0.656 |

- **The reranker lowers well-phrased queries' scores:** −0.115 [−0.188, −0.052] for jina-code-v2
  (no question gained, 11 lost) and −0.094 [−0.156, −0.031] for Qwen3. It reads 192 tokens of
  query and code together, so a two-part query leaves it little of each candidate, and the set is
  built around look-alikes and behaviour past the start of long definitions.
- **On the original wording it adds +0.052 and +0.042, neither significant.**
- **On CPU it costs 40×:** jina-code-v2 on tantivy's hard questions takes 65 ms per search
  without it and 2.6 s with it (top 8 0.969 and 0.875); without it, CPU and Metal scores match.
- Against the previous default (Qwen3 with the reranker), jina-code-v2 without it scores +0.188
  [+0.115, +0.271] on combined queries (18 gained, none lost) and −0.010 on the original wording.

### Decisions

- **Default embedder is now jina-embeddings-v2-base-code on every backend** (Metal, CPU and the
  fastembed fallback). It is the most accurate model on the hard set, indexes 2.5× faster than
  Qwen3 on Metal, and on CPU runs about 16% slower than CodeRankEmbed for +0.10 top 8.
- **The reranker is off by default.** `[rerank] enabled = true` or `--rerank` turns it on;
  `--reranker` and `--rerank-candidates` now imply `--rerank` (they were ignored once the default
  changed).
- **The search tool asks for combined queries:** one sentence on the behaviour, then guessed
  names. It previously said to avoid guessed names.
- Upgrading: indexes built with Qwen3 or CodeRankEmbed are reported stale until
  `lode index --force` re-embeds them, or set `model` to keep the old one.

## Agent situations and the lodestone plugin

`plugin/` packages lodestone for Claude Code: a `lodestone` skill (when to search in each
situation, the query format, when to use Grep instead) and the MCP server config.
`eval/situations/` measures how agents use it: write, review, refactor and locate tasks on
stacks-core, each run headless on Sonnet without lodestone, with its tools, with the tools and the
skill, and with a SessionStart reminder added (624 runs, $30; method and full table in
`eval/situations/README.md`).

| Situation | Correct without lodestone | With tools + skill | With the reminder | Runs that searched (skill / reminder) |
| ---- | ---- | ---- | ---- | ---- |
| write | 25/54 | 24/54 | **37/54** | 6% / 50% |
| review | 33/36 | 34/36 | 33/36 | 31% / 69% |
| refactor | 30/30 | 30/30 | 30/30 | 43% / 97% |
| locate | 36/36 | 36/36 | 36/36 | 8% / 3% |

- **Agents do not discover lodestone from the tool or the skill alone**; the skill was never
  loaded in any run. A one-paragraph SessionStart reminder makes them search where it helps, with
  well-formed queries, and leaves locate (where Grep suffices) alone.
- **On write tasks the reminder raises correctness by +0.22 per task [+0.09, +0.37], but not
  because of lodestone.** A control with the same reminder and no lodestone (Grep only) scored
  the same, 37/54 (+0.222 [+0.074, +0.389] over no reminder); the shipped plugin scored 34/54
  (−0.056 against the control, n.s.). Effort to find the helper was also the same: about one
  tool call. stacks-core's helper names are predictable and largely known to the model, so Grep
  with guessed names is as fast as lodestone here. Review, refactor and locate are at the ceiling
  in every arm, so they show adoption and cost, not accuracy.
- **Side effect:** with lodestone's prompts present, stacks-core's own review skill loads in 3–9
  of 36 review runs instead of 31.
- **The reminder now ships in the plugin** (`plugin/hooks/`, a SessionStart hook). It stays silent
  unless a running daemon serves the session's repository, so it never points agents at a tool
  that is not there. The skill-only variant stays in `eval/situations/plugin-skill/` for the
  experiment's `skill` arm.
- **The skill's description is now request-matching** ("Use when asked to write, add or implement a
  function, … or to review, refactor or deduplicate code"). No description got the skill loaded
  (0 of 216 runs), but the listing itself steers agents: on write tasks this wording made them
  search in 48% of runs and lifted correctness to 36/54 (+0.204 [+0.056, +0.389]), about what the
  reminder does; the original "Use before writing…" phrasing reached 6% and 24/54. The skill-only
  arm (`plugin-skill/`) now uses the new description too.
- **The reminder stays for now.** Without it, v4 searches in 44% of review and 77% of refactor runs
  against 69% and 97% with it (accuracy is at the ceiling in both), and both crowd out
  stacks-core's own review skill (loaded in 9 and 3 of 36 review runs, against 31 without
  lodestone). Since a lodestone-free reminder does as well on write tasks, the agent-level case
  for lodestone itself is unproven; it needs tasks where guessing names fails (the hard set).

## Agent situations on Opus 5.5

**Corrected:** the first run's LibreChat plugin arm searched the stacks-core index (a shared MCP
config file; `run.py` now writes one per port). After re-running it (28 runs, all searches on the
LibreChat index):

| Positives correct (48 runs per arm) | No reminder | Advice only, Grep | Advice + lodestone (plugin) |
| ---- | ---- | ---- | ---- |
| stacks-core + LibreChat | 27/48 | 37/48 | **43/48** |
| Cost per correct answer | $0.296 | $0.227 | $0.194 |

Plugin vs advice only: +0.125 per task [+0.000, +0.271] (5 gained, 1 lost), +0.208 [+0.042,
+0.417] on LibreChat alone; wall time ×0.93 [×0.89, ×0.98], cost ×0.97 [×0.91, ×1.03]. The table
and bullets below are the original figures with the invalid LibreChat arm.

28 realistic write, review, refactor and locate tasks on stacks-core and LibreChat (hard targets,
no reuse wording), Opus 5.5 at medium effort, 2 runs each, 168 runs; method and tables in
`eval/situations/README.md`.

| Positives correct (48 runs per arm) | No reminder | Advice only, Grep | Advice + lodestone (plugin) |
| ---- | ---- | ---- | ---- |
| stacks-core + LibreChat | 27/48 | 37/48 | **41/48** |
| Cost per correct answer | $0.296 | $0.227 | $0.210 |

- **The plugin's reminder now leads with the advice**: look for existing code and reuse what fits,
  then point at lodestone. The `remind` control uses the same advice word for word, so the two
  arms differ only in lodestone.
- **Lodestone adds a little correctness and no efficiency on Opus.** Plugin vs advice only: +0.083
  per task [−0.021, +0.229] (3 tasks gained, 1 lost); cost ×1.01, wall time ×0.97, tool calls
  ×1.12 per task. Advice vs nothing: +0.208 [+0.062, +0.375]. Most of the gain is the advice.
- **The model matters more than the wording.** In a 27-run pilot, Sonnet at medium effort used the
  plugin's search in 2 of 9 runs, Sonnet at high in 5, Opus 5.5 at medium in 9; Opus searched in
  51 of 56 runs here. Subagents were never used (0 of 222 runs with the Agent tool), and an Explore
  subagent would get lodestone's tools but not the reminder.
- **Harness fix:** `claude -p --setting-sources project` still loads `~/.claude/CLAUDE.md`, so all
  earlier agent runs saw the author's global instructions (the same in every arm). `run.py` now
  excludes it.
- **On a private repository** (a 185k-line Rust workspace the model cannot have seen; 12 tasks,
  48 runs, advice vs plugin), lodestone found the answer with half the tool calls and ×0.67
  [×0.50, ×0.87] the tokens, but whole runs cost about the same (×0.95 [×0.85, ×1.06]), and
  correctness was at the ceiling in both arms (19/20 vs 20/20). Details in
  `eval/situations/README.md`.
- **Replaying the agents' 78 lodestone queries** puts the answer in the top 8 for 59. The query
  format is fine (combined beats sentence or names alone); the reranker (+1) and a top 16 (+3)
  barely help. The misses come from searching for the surrounding feature instead of the piece
  that already exists, and from vocabulary gaps; see `eval/situations/README.md`.

## Two MCP search tools, 24 results

- MCP `search` is replaced by `semantic_code_search(query, example_identifiers[])`, which joins
  the sentence and the guessed names into the query form the index was tuned on, and
  `similar_code_search(code)`, which searches with one function's source, strips diff prefixes
  and leaves out the passed code's own definition (ignoring whitespace and comments).
- `DEFAULT_TOP_K` 8 → 24 (MCP, REST, CLI); the MCP tools no longer take `top_k`.
- The skill and SessionStart reminder name the tool for each situation.
- Opus 5.5, 27 tasks, one run each: correct 23 → 25/27 (+0.074 [+0.000, +0.185]), tool calls
  ×0.88 [0.78, 0.98], tokens ×0.90; billed cost ×1.14 [1.04, 1.26], mostly cold prompt-cache
  starts in the new arm (×1.04 [0.90, 1.21] priced as warm starts); `similar_code_search` used in
  9 of 18 review and refactor runs; no false equivalents on controls. Offline, 16 results instead
  of 24 would save about 0.5% of a run's tokens and lose 5 of 78 replay hits, so 24 stays.
  Details in `eval/situations/README.md`.

## `lode install` for Claude Code and Codex

- `lode install` / `lode uninstall` (`--client claude|codex`, `--scope user|project`,
  `--dry-run`) add the skill, the MCP server and the SessionStart reminder to each harness whose
  CLI is on PATH. The skill goes to the shared `.agents/skills/lodestone/`, which Codex reads at
  both scopes; Claude Code 2.1.301 reads `~/.agents/skills` but not a repository's
  `.agents/skills` (checked from its startup event), so it gets a symlink from `.claude/skills/`.
  MCP servers use `claude mcp add` / `codex mcp add` (user) or `.mcp.json` /
  `.codex/config.toml` (project); hooks go to `settings.json` / `hooks.json`. Re-running is
  idempotent and other hooks and keys are left in place.
- `lode hook session-start` replaces the plugin's shell script (and its `curl` dependency): it
  reads the session's `cwd` from the hook's stdin and prints the reminder only when the daemon
  serves it. The skill and reminder text are built into the binary.
- Checked against the real CLIs in a temporary `HOME`: `codex mcp get` and `claude mcp get` list
  the server, and a Claude Code session there starts with the reminder, the skill and the three
  tools connected. Codex's hook trust (`/hooks`) was not exercised.

## On-demand daemon

- The MCP server is now `lode mcp`, a stdio relay that harnesses start per session, in place of
  the HTTP URL. It starts `lode serve --on-demand` in the background when no daemon answers
  (like sccache), waits for it, takes a lease on the session's worktree (renewed every 30 s,
  released when the harness closes stdin), and relays tool calls to a new `POST /v1/tool`. A call
  without `repo` searches the session's worktree, so several registered repositories no longer
  need `repo` on every call. If the daemon dies mid-session, the relay starts it again and
  retries once.
- An on-demand daemon watches only leased worktrees: a session lease (`POST /v1/sessions`,
  `DELETE /v1/sessions/{id}`), or a search, register or index request (a lease of 10 min, at
  most the idle timeout). Taking a lease catches the worktree up, and unleased worktrees stop
  being watched. The daemon exits `idle_timeout_secs` (default 1800, 0 = never) after the last
  lease ends. Plain `lode serve` keeps watching everything and runs until stopped.
- The daemon listens on a Unix socket, `<data dir>/daemon.sock` (`socket`, `LODESTONE_SOCKET`),
  with mode `0600`, so other local users and web pages (DNS rebinding) cannot reach its
  unauthenticated API, and users sharing a machine no longer contend for one port. TCP on
  `127.0.0.1` is off unless `port` (or `LODESTONE_PORT`, `lode serve --port`) is set; HTTP MCP
  clients and the eval harness need it. Windows has no Unix sockets and keeps TCP on 4600. The CLI
  talks HTTP over the socket through a small ureq connector.
- A lock file next to the socket makes one daemon per socket. The daemon takes it, and binds,
  before loading the model, so a second daemon started at the same time exits at once. Holding
  the lock, it replaces a socket file a crashed daemon left behind, and it removes its own on
  exit. Data directories whose socket path would exceed the ~104-byte limit get a short path in
  `$XDG_RUNTIME_DIR` or the temp directory instead.
- `lode hook session-start` decides from the registry when no daemon runs, and starts one in the
  background. With a daemon running, it takes a 5-minute lease, so catch-up starts as the
  session opens.
- `lode install` registers `lode mcp` (`claude mcp add … -- <lode> mcp`, `codex mcp add … --
  <lode> mcp`, or `"command": "lode", "args": ["mcp"]` in `.mcp.json` and `.codex/config.toml`).
- `GET /v1/search` defaulted to 8 results; it now uses the shared default of 24.
- Watchers can be removed; a polling watcher's timer no longer outlives its worktree.
- Checked:
  - Integration tests cover an on-demand daemon that watches a leased worktree, unwatches it
    after release and exits when idle, and a `lode mcp` that starts the daemon itself and
    searches the session's worktree with two repositories registered, both over the socket, and
    a second daemon refused on a live socket after the first replaced a stale socket file.
  - In a real Claude Code session with no daemon running, the hook started one and the relay
    connected in 0.5 s. The first search returned 2.8 s after the session started, model load
    included.

## CLI starts the daemon

- `lode search`, `register` and `index` start an on-demand daemon when none runs, as `lode mcp`
  does, instead of running in-process: a search no longer loads the model each time, and
  `register` returns while the daemon indexes. Each call renews the repository's lease, so the
  daemon stays up while the CLI is in use. `status` never starts one; `--local` keeps
  everything in-process.
- Starting fails fast: if the started daemon exits and no other daemon holds the socket's lock
  (a lost race), the command fails at once and points at `daemon.log` instead of waiting 120 s.
- An on-demand daemon no longer exits mid-index: the idle exit waits for running indexing passes,
  so `lode register` on a large repository finishes even after its lease ends.
- `lode search` returns 24 results by default (`--top-k`), like MCP and REST; it was 8.
- README: the missing built-in models, the `remote` backend and the routes `lode mcp` uses.
- Checked: integration tests cover a CLI that registers and searches through a daemon it
  started (which then exits when idle), `status` leaving no daemon behind, and a daemon that
  cannot start failing the command in about a second.

## Clarity

- Built-in Clarity support (`lang-clarity`, on by default) for `.clar` files. The grammar,
  [xlittlerag/tree-sitter-clarity](https://github.com/xlittlerag/tree-sitter-clarity) (MIT,
  Clarity 6), is not on crates.io, so its generated parser is vendored in
  `vendor/tree-sitter-clarity/` and compiled by a new `build.rs`.
- `queries/clarity/tags.scm` makes one item per `define-public`, `define-read-only`,
  `define-private` (`function`), `define-trait` (`interface`) and its signatures (`method`),
  `define-constant`, `define-data-var`, `define-map` and token definitions. The `;;` comments
  directly above a definition are its doc; a file header separated by a blank line is not.
- Checked on stacks-core `95c6a45eed`: all 1,935 definitions in its 120 indexed `.clar` files
  became items, plus 20 trait signatures. The only misses are two type-checker fixtures that are
  deliberately invalid Clarity. (`contrib/.../pox-5.clar` is a symlink; it is indexed once, at
  its real path.) Existing indexes pick up `.clar` files on their next scan.

## Watcher follows the full scan's ignore rules

- Watched changes used a smaller rule set than full scans: the root `.gitignore` and
  `.lodestoneignore` and `.git/info/exclude` only. Files excluded by a nested `.gitignore`, the
  global git excludes file or a hidden directory were indexed when they changed, e.g. edits in
  agent worktrees under a globally ignored `.worktrees/`, and the index then reported itself stale
  until the next full scan removed them. Changed files are now checked by the full scan's own
  walker, entering only the directories that lead to them, so both apply the same rules. Symlinked
  files changed while watched are no longer indexed either.
- Checked: an integration test writes files excluded each way, plus one indexable file, into a
  watched worktree; only the indexable one is indexed. It fails on the previous code.

## `lode transcript-stats`

- Reports how agents use lodestone from Claude Code (`--claude [DIR]`) and Codex
  (`--codex [DIR]`) transcripts, per registered repository and harness: sessions that searched,
  search before the first grep, searches with a result later read, searches with no results,
  latency, and grep / glob / read calls. `--repo`, `--since`, `--queries`, `--json`.
- Reads only sessions in registered repositories. Claude project directories are named after
  their working directory, so others are never opened; a Codex session elsewhere is read up to its
  first line. Claude subagent transcripts count towards their parent session, as do Codex
  subagent threads.
- Incremental: `transcripts.sqlite` records each transcript's read offset (complete lines only),
  size, mtime and a hash of its first 4 KB. Unchanged files are skipped without opening, grown ones
  read from the offset, rewritten ones re-read; a session registered later is picked up; a parser
  version bump re-reads everything. Extracted calls outlive the harnesses' transcript cleanup.
- `registry::root_of` matches a directory that no longer exists (a removed worktree) as given.
- Checked: integration tests on synthetic transcripts of both formats cover the statistics,
  skipping unregistered projects, unchanged files, appended and half-written lines, rewrites and
  late registration. On this machine's real transcripts (182 files, 57,516 lines) the first run
  took 2.3 s in a debug build and the second 22 ms, reading nothing.

## `lode stop`

- `lode stop` asks the running daemon to shut down (`POST /v1/shutdown`, over the socket) and
  waits until it has exited and released its socket; with none running it says so and succeeds.
- The daemon now handles SIGTERM like Ctrl-C. Before, `kill` ended it at once, cutting off
  in-flight requests and leaving its socket file behind.
- Shutting down finishes in-flight requests, then exits without waiting for an indexing pass in
  progress (it waited for the whole pass before); the index is consistent after every batch, and
  the next pass carries on.
- Checked: an integration test stops a daemon with `lode stop` (clean exit, socket removed, a
  second `lode stop` reports none running) and sends SIGTERM mid-index: a clean exit within 3 s.
  Without the change, exit waited out the 6 s indexing batch.

## Worktrees never borrow another checkout's index

- A session, CLI command or `repo` path was served by the innermost registered root containing
  it, or, with one repository registered, by that one. A linked git worktree inside a registered
  checkout (agents put them in `.claude/worktrees/`, `.worktrees/`) or any unregistered checkout
  therefore searched the main checkout's index: code the worktree deleted still showed up, code
  it added did not.
- `registry::place` now decides from git itself (`.git` files, `commondir`): a linked worktree is
  served only by its own index, and one of a registered repository is registered on first use
  (`auto` in `registry.json`) and indexed, mostly from the shared embedding cache. A separate
  repository nested in a registered checkout is not served by it; submodules still are.
  Unregistered checkouts get an error naming `lode register`. Only outside any checkout does the
  single registered repository serve by default.
- The CLI's `search` and `status` default to the current directory under the same rules. A
  search naming an unregistered path is an error instead of an empty, stale answer.
- Automatically registered worktrees whose directory is gone are unregistered and their index
  deleted, at daemon start and every minute.
- The session hook shows its reminder in linked worktrees of registered repositories.
- Checked: an integration test with real `git worktree`s covers the hook, a session in a nested
  worktree seeing only its own changes, the main checkout unaffected, an unrelated checkout
  getting an error (relay and CLI), and pruning after `git worktree remove`. It fails on the
  previous code.

## Codex approves lodestone's tools

- Codex's auto-approver denied lodestone tool calls. `lode install` now writes
  `default_tools_approval_mode = "approve"` into `[mcp_servers.lodestone]`, at user scope
  (`~/.codex/config.toml`) as at project scope, editing the file directly instead of running
  `codex mcp add`, which cannot set it. An entry from an older install is replaced in place;
  uninstalling removes it and leaves the rest of the file alone.
- All three MCP tools are annotated `readOnlyHint: true`, `openWorldHint: false`: they only read
  the index.
- Checked: install tests cover replacing an older user entry and uninstalling; Codex 0.167.1 loads
  the written config and reports `default_tools_approval_mode: approve` (`codex mcp get`), and
  rejects unknown values (`auto`, `prompt`, `writes`, `approve` are valid). The relay test checks
  the annotations.

## Transcript stats count calls where they worked

- `lode transcript-stats` showed no lodestone use for a Codex thread that searched concurro
  16 times: the thread started in a scratch directory (`~/Documents/Codex/…`), and other concurro
  sessions in the folder above the repository, while the work happened in Codex worktrees
  (`~/.codex/worktrees/<task>/concurro`). Sessions were attributed only by their starting
  directory, and skipped after their first line.
- Each call now counts towards the registered repository it worked in: its `repo` argument
  (name or path), file path, or command directory, else the session's working directory; a
  lodestone search naming none falls back to the one repository the session's other calls worked
  in. Linked worktrees count towards their repository's main checkout (by git common directory).
  A session counts for every repository it worked in.
- Every transcript is read; calls in no registered repository are not kept. Registering a
  repository (as identified by git, so auto-registered worktrees don't count) re-reads
  everything. The parser version bump re-reads once.
- Newer Codex records command directories as `file://` URLs; they are decoded.
- `--since` filters calls by time, so a long-running thread's recent searches count.
- Failed (or denied) searches are reported apart from empty ones, and leave latency alone.
- Checked: a new test covers sessions starting outside the repository that search it by name
  and path, read its files and run commands in it, a failed search, and `--since` on a thread
  started the day before. On this machine: concurro shows 16 Codex searches (6 failed, 1 empty,
  9 with a result read), matching Codex's own thread history. A first full read of 410 transcripts
  (427k lines) took 19 s in a debug build; the next run 0.04 s.

## `similar_code_search` takes several definitions

- `code` is now a list: every function a diff adds (or a refactor moves) in one call. Each
  definition is searched separately and gets its own section, headed by its first line, with its
  own match left out. One definition gets 24 results as before; several share them, at least 5
  each. A single string is still accepted.
- Why: in a real Codex session the skill's "once for each function" produced 13 calls in 40 s
  returning ~46k characters, none of which the agent read further. Batched, that is one call and
  65 result lines instead of 312.
- The skill, the session reminder and the tool description now ask for one call with every
  definition. `transcript-stats` shows a batched query as `[n] first line | first line …`; the
  eval scorer reads list-valued `code`.
- Checked: the MCP test passes two definitions (one with a diff `+`) and gets two sections of at
  most 12 results, each leaving out its own definition.

## Searches index the worktree they name

- A linked worktree that a session reached only through `repo` (Codex sessions often start
  outside the repository) was registered but never indexed: MCP calls (`/v1/tool`, `/mcp`) ran
  the search without the per-request activation REST searches get, and an always-on daemon
  skipped activation altogether. Every search now activates the worktree it names: watched,
  caught up, indexed if new; on-demand daemons also lease it. The same gap left a registered but
  unwatched repository uncaught-up when only named through `repo`.
- Queueing a full scan marks the worktree pending at once, so a search right after reports it.
  An empty answer from an index still being built now says so ("still being indexed; search
  again shortly") instead of "no matches", which agents read as "nothing exists". The
  never-indexed reason no longer tells you to run `lode register` for a registered worktree.
- Linked worktrees are named after their repository, `<repository>:<worktree>` (e.g.
  `concurro:limits-cleanup`), instead of the directory name alone, which collided across
  repositories (`limits-cleanup-2`). Earlier automatic registrations are renamed when the daemon
  tidies the registry (start-up, every minute), alongside pruning removed worktrees.
- Checked: a test searches a new nested worktree through `lode mcp` from outside every
  repository, by path and then by its `repo:wt` name; the first answer is never "no matches", the
  worktree gets indexed, and an old-style name is renamed on the next start. It fails on the
  previous code ("no matches", never indexed).

## `lode index` and `lode register` default to the current directory

- `lode index` without a path re-indexes the registered worktree containing the current directory
  (a linked worktree of a registered repository is registered first); elsewhere it refuses and
  suggests `lode register`. `lode register` without a path registers the current directory's git
  checkout, not the subdirectory you are in.
- A path given to `lode index` (CLI, in-process, or `POST /v1/index`) means the registered
  worktree containing it: `lode index .` in `core/` used to register `core` as a new repository.
  Only a path outside every registered worktree is registered as given, as before.
- Checked: a test runs `index` and `register` from subdirectories, in-process and through the
  daemon, and checks that only the checkout is ever registered.

## Sessions keep the worktrees they search watched

- A session's tool call that names a worktree with `repo` now holds that worktree for as long as
  the session's lease lives. Before, only the 10-minute request lease did: a Codex session (started
  in its own directory, so its lease holds no worktree) whose searches were further apart found
  the worktree unwatched each time, paid a full rescan, and got `stale: pending: full rescan` on
  nearly every result.
- Checked: a test searches a worktree from a session outside it, waits past the request lease and
  several sweeps, and finds it still watched until the session is released. It fails on the
  previous code.

## CI and crates.io

- The crate is published as `lodestone-cli` (`lodestone` and `lode` are taken on crates.io); the
  library keeps the name `lodestone` and the binary `lode`. Until the first release is on
  crates.io, the README installs from the git repository.
- CI (`.github/workflows/ci.yml`) on pushes to `main` and on pull requests: `fmt --check`,
  `clippy -D warnings`, `nextest` on Linux and macOS, and `cargo package`.
- A `v*` tag runs CI and then `cargo publish` with crates.io trusted publishing
  (`.github/workflows/publish.yml`). `eval/`, `examples/` and `PLAN.md` stay out of the package.
- `branch_switch_rescans_with_cache_hits` failed consistently in the full suite: a branch switch's
  file changes and ref change can land in separate debounce windows, so the last pass is a no-op
  rescan and its report replaced the one the test read. It now checks that switching back adds
  nothing to the embedding cache. Eight full-suite runs passed in a row (it had failed 3 of 8).

## The reminder reaches desktop sessions

- Claude Code in the desktop app never ran the SessionStart hook at startup (only after a
  compaction), so new Code-tab sessions got lodestone's tools but not the reminder to use them.
  `lode install` now also adds a `UserPromptSubmit` hook for Claude (`lode hook prompt-submit`,
  and in the plugin) that adds the reminder on a session's first prompt. Whichever hook checks a
  session first leaves a marker in `~/.lodestone/hook-sessions/`; the prompt hook stays silent
  once one exists, so CLI sessions still get the reminder once. Markers older than 30 days are
  removed. Codex runs SessionStart at startup and is unchanged.
- Checked: a test runs both hooks with session ids, and checks that the prompt hook adds the
  reminder only to a session no hook has reminded, and only once.

## Desktop worktree sessions search their worktree

- Claude Code in the desktop app starts MCP servers in the main checkout (`cwd` and
  `CLAUDE_PROJECT_DIR`) even when the session runs in a worktree, so `lode mcp` searched the main
  checkout's index there and missed the branch's changes. The hooks get the session's real
  directory; they now record it in the session's marker (JSON: `event`, `cwd`), and `lode mcp`
  uses it for the session `CLAUDE_CODE_SESSION_ID` names, re-leasing before a call when it
  changed. Without a marker it keeps its own directory, as before. Markers from the previous
  build (the event name only) still count as reminded.
- Claude Code also offers MCP roots (the CLI reports the session's directory); not used yet,
  since what the desktop app reports there is unverified.
- Checked: a test starts the relay in the main checkout for a session whose hook recorded a linked
  worktree, and finds the worktree's code; a session with no marker still gets the main
  checkout's. It fails on the previous code.

## Concurrent searches no longer fail

- usearch gives each index a fixed number of thread slots (one per core by default) and fails a
  search that finds them all taken ("Reserve capacity ahead of searches!"). More concurrent
  searches than cores, e.g. several sessions on a small machine or the 8-client test on a 3-core CI
  runner, occasionally got HTTP 400. Each index now reserves one slot per core explicitly
  (including loaded and duplicated ones), and searches beyond that wait for a slot.
- `branch_switch_rescans_with_cache_hits` asserted that `helper_3` ranks first for "helper_3", but
  the test embedder hashes "3" and "44" into the same bucket, so `helper_3` and `helper_44` have
  identical vectors and their order depends on insertion order. It now checks presence.
- Checked: a unit test runs four searches per core at once on a duplicated index; it fails on the
  previous code (most searches errored with 64 threads on 16 cores).

## Svelte components

- `.svelte` files were skipped, so in a Svelte app the agent could not find anything defined or
  rendered in a component. In one SvelteKit app that was over a quarter of its definitions plus
  all of its markup, and a component-local helper duplicating a shared one was invisible to
  `similar_code_search`.
- New `lang-svelte` feature (on by default), using `tree-sitter-svelte-ng` only to find `<script>`
  and `<style>` blocks. Script blocks are tagged with the TypeScript grammar (`lang="ts"`) or
  the JavaScript one, the markup is chunked as text, and styles are left out. Each part is the
  file with everything else blanked byte for byte, so the existing tagger runs unchanged and
  every item keeps the component's own lines and columns. `tree-sitter-htmlx-svelte` (Svelte 5)
  could not be used: it requires tree-sitter 0.26, and only one tree-sitter runtime can link.
- Text chunks: a paragraph longer than the chunk budget is now cut at line boundaries instead of
  truncated, which matters for markup that has few blank lines. Indexes re-chunk once.
- Checked: chunk tests for a component with module and instance TypeScript scripts (positions,
  doc comments, markup without script or style), a plain-JavaScript one and one without a script,
  and for paragraph cutting. On a SvelteKit app with 47 components, all were indexed in under
  4 s, and searches for a helper defined in a component, or for the component rendering a given
  view, now find it.

## Path filters, applied inside the search

- Agents could narrow a search by language or build unit, but not by directory: the CLI's
  `--path-prefix` was not an MCP input, and nothing took a glob. Both searches now take `paths`,
  repo-relative globs (`crates/foo`, `**/*.rs`, `!**/generated/**`). A pattern also matches
  everything under a directory it names, and `!` excludes. The CLI's `--path` (repeatable)
  replaces `--path-prefix`, which remains as an alias; REST takes `paths` (POST) or one `path`
  (GET).
- Every filter (`paths`, `language`, `from_unit`, tests, kinds) used to run after fusion, on the
  top 200 candidates from each list, so a narrow scope in a large repository could return only a
  few results, or none, even with better matches further down. Filters now run inside the
  searches: usearch's `filtered_search` walks the graph past rejected items until it has enough
  admitted ones, and tantivy's `FilterCollector` drops rejected ids before ranking. Both use one
  predicate.
- Tool inputs carry their own instructions: the search tools' descriptions now say only what the
  tool does and returns, and each input's description says how to fill it. `repo` said a missing
  value meant the only registered repo, but `lode mcp` uses the session's worktree. `language`
  now lists names and says an unknown one returns nothing, `from_unit` says it includes the
  unit's dependencies, and `include_tests` and `status`'s `repo` gained descriptions. A test
  requires a description on every input.
- Checked: with one candidate per list, filtered searches for a directory, a glob and a set of
  exclusions each find an item that unfiltered search ranks below 20 others (the test fails when
  filtering runs after fusion); a unit test finds k admitted vectors past rejected nearer ones;
  an invalid glob is reported.

## Sessions in parent folders, local tools, new worktrees

Transcript stats after the reminder fix showed four problems.

- **Parent folders.** Sessions often start in a folder that holds several checkouts rather than in
  one. Neither hook reminded such a session (one ran about 1,000 grep/rg commands without a
  search, reminded only after compactions moved it into a worktree), and calls without `repo` got
  an error suggesting `lode register`. The hooks now remind a session in a folder holding
  registered worktrees, listing them by name and root, and the error for a call without `repo`
  lists them too (the relay sends the session's directory with each call).
- **Approvers.** Codex's automatic approval review rejected six searches in one session as sending
  private code "to an external service", then steered the agent away from lodestone. When
  embeddings are computed on this machine, the search tools' descriptions now say it runs locally
  and sends nothing elsewhere (`Config::is_local`: any backend but `remote`, or `remote` on a
  loopback address).
- **New worktrees.** Five searches, mostly the first in a worktree Codex had just created,
  answered "still being indexed; search again shortly". A search in a worktree that has never
  finished an index now waits up to 30 s for its first one (it stops waiting if indexing fails).
- **Stats.** `lode transcript-stats` counted five searches with results as having none: Codex
  records some results as a text block holding the whole serialized result. Those are unwrapped,
  and the parser version is bumped, so transcripts are read again once.
- Checked: in the worktree test, a hook and a relay call in the repository's parent folder name it;
  the first search naming a new worktree now returns its results (it failed before the wait);
  unit tests for loopback detection, the tool descriptions and the unwrapping. The hook, run for a
  real parent folder, lists its four registered worktrees.

## Claude Code loads the search tools upfront

- After the session fixes, Claude still never searched, though the reminder reached its sessions
  and Codex searched in each session it spent on code. Claude Code lists MCP tools as deferred:
  the model must load one with ToolSearch before calling it. Claude did so 14 times in one long
  session, each time for a tool the app's own instructions named, and never for lodestone, which
  the reminder named without saying how to load it.
- Both search tools now set `_meta["anthropic/alwaysLoad"]` in `tools/list`, so Claude Code
  (v2.1.285 or later) loads them upfront; `status` stays deferred. A user's server-level
  `alwaysLoad` setting still overrides this.
- For older versions, or a configuration that defers the server, the reminder and the skill also
  say to load the tools with ToolSearch (`+lodestone search`, which matches the plugin's prefixed
  names too) when they are listed only as deferred.

## Cues that fit the session, and stats that separate exposure from choice

Transcripts after the upfront-loading change still showed no Claude searches. Reading them:

- **Exposure.** Before `50a6882` every Claude session had the search tools deferred, and desktop
  sessions often got the reminder only at a resume or compaction. Two sessions have had both the
  tools and the reminder from the start; neither searched.
- **Wording.** The reminder named a `Grep` tool the desktop app does not have (its searches are
  `grep` in Bash), led with reuse before writing code when most sessions investigate or review,
  and in a folder of checkouts opened with "not itself indexed". It now leads with locating
  code by behaviour instead of `grep -r` with guessed names, says "text search (grep, rg)", and
  opens the folder variant with what is indexed. The skill says the same.
- **Subagents.** About a third of the grep commands ran in subagents, which have the tools but
  none of the session's reminders. `lode install` adds a `SubagentStart` hook for Claude
  (`lode hook subagent-start`, also in the plugin) that adds the reminder to each one and leaves
  the session's marker alone.
- **Stats.** A Claude shell command counted as a grep whenever `grep` appeared in a pipeline;
  roughly a quarter of those filtered test or build output. Only a search over files counts now.
  The report also gives, from Claude's `prompt_snapshot` and `deferred_tools_delta` attachments,
  the share of sessions with the search tools loaded and how many of those searched. The parser
  version is bumped, so transcripts are read again once.
- Checked: unit tests for the shell classification and the attachments; the transcript test
  counts a session listed as deferred then loaded, and skips a piped grep; the daemon test
  reminds a subagent twice in a session already reminded; the install test writes the hook.
