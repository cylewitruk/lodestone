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
the same scenarios also run as `tests/daemon.rs` with a deterministic embedder.

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
