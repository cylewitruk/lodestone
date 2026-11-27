# Decision log

Why the defaults are what they are, with the measurements behind them and the alternatives
rejected. Each section leads with the current choice. User-facing changes per release are in
[CHANGELOG.md](../CHANGELOG.md); how to rerun the measurements is in
[evaluation.md](evaluation.md).

Unless noted: Apple M4 Max (16 cores, 64 GB), stacks-core `95c6a45eed` (about 27k definitions).
"Top 8" or "top 24" is the share of questions whose answer is among the results an agent sees;
comparisons are paired per question with 95% bootstrap intervals (`eval/compare.py`). The
original question set has 137 answerable questions over stacks-core, tantivy and LibreChat; the
hard set has 96 (see [eval/README.md](../eval/README.md)).

## Embedding model and backend

**Current:** jina-embeddings-v2-base-code on every backend; candle, on Metal on macOS and on the
CPU elsewhere; f32 vectors. fastembed is an opt-in build feature.

The hard set decided it (combined queries written by rewriters told to invent every name; no
reranker):

| Embedder (top 8) | Original question | Combined | Combined, names removed | Combined, recalled names | vs Qwen3, combined |
| ---- | ---- | ---- | ---- | ---- | ---- |
| **jina-embeddings-v2-base-code** | **0.646** | **0.948** | **0.948** | **0.969** | +0.094 [+0.042, +0.156] |
| Qwen3-Embedding-0.6B | 0.615 | 0.854 | 0.854 | 0.948 | — |
| CodeRankEmbed | 0.542 | 0.844 | 0.844 | 0.948 | −0.010 [−0.073, +0.052] |
| granite-embedding-english-r2 | 0.479 | 0.750 | 0.729 | 0.927 | −0.104 [−0.188, −0.021] |
| UltraBERT-Embed-150M | 0.427 | 0.708 | 0.698 | 0.896 | −0.146 [−0.250, −0.042] |
| granite-small-r2 | 0.396 | 0.698 | 0.698 | 0.896 | −0.156 [−0.250, −0.073] |

- jina-code-v2 finds 9 answers Qwen3 misses and misses none Qwen3 finds, in all three
  repositories and under every hard-set criterion. Its MRR is the same (0.697 against 0.688).
- Behaviour past the embedded text (40 questions) is the hardest case: 0.88 for jina-code-v2,
  0.78 for Qwen3, 0.72 for CodeRankEmbed.
- On the original set (no hard criteria) Qwen3 led on poorly phrased questions by about 0.11, but
  with combined queries every model reached 0.94–0.985 and could not be told apart. That set is
  saturated; use the hard set to compare models.

Speed and licence, from the model comparison on the original set:

| Model | Licence | Top 8, original wording | Chunks/s, Metal | Chunks/s, CPU |
| ---- | ---- | ---- | ---- | ---- |
| Qwen3-Embedding-0.6B | Apache-2.0 | 0.825 | 42.7 | 6.6 |
| jina-code-embeddings-0.5b | CC-BY-NC-4.0 | 0.752 | 58.1 | 6.8 |
| harrier-oss-v1-0.6b | MIT | 0.781 | 32.2 | — |
| granite-embedding-english-r2 | Apache-2.0 | 0.723 | 57.8 | 10.9 |
| **jina-embeddings-v2-base-code** | Apache-2.0 | 0.715 | 107.1 | 14.1 |
| granite-embedding-small-english-r2 | Apache-2.0 | 0.679 | 134.9 | 27.9 |
| UltraBERT-Embed-150M | Apache-2.0 | 0.730 | 93.3 | 13.3 |
| CodeRankEmbed | MIT | 0.701 | 106.2 | 16.8 |
| all-MiniLM-L6-v2 | Apache-2.0 | 0.628 | 205.8 | 78.3 |
| EmbeddingGemma 2 | Apache-2.0 + Gemma use policy | 0.672 | 43.9 (f32) | 10.5 |

jina-code-v2 indexes 2.5× faster than Qwen3 on Metal; on CPU it is about 16% slower than
CodeRankEmbed. At Qwen3's 6.6 chunks/s, a 27k-definition repository takes about 70 minutes on
CPU.

- **Previous defaults:** CodeRankEmbed (the original 20-question spike), then Qwen3 on Apple GPUs
  with CodeRankEmbed on CPU (the original set). Both were replaced by the hard set's result.
- **candle over fastembed:** candle on Metal embedded 44.7× faster than fastembed on CPU for
  jina-code and 9× for bge-small; fastembed pads every batch to `max_length` and peaked near
  40 GB RSS. candle runs every built-in model on every platform, so fastembed became opt-in,
  which also saves 28.6 MB of binary.
- **f32 vectors:** f16 lowered rank@5 by 0.05 (CodeRankEmbed); f32 for 27k × 768 is 84 MB.
- **Ports are checked:** every model matches its reference implementation (transformers or the
  vendor's pinned code) to three decimals. jina-code-v2 needed its own module
  (`src/embed/jina.rs`) because the checkpoint is a qk-post-norm JinaBERT variant candle lacks;
  ModernBERT, EmbeddingGemma 2 and UltraBERT are vendored too. bge-small and EmbeddingGemma 2
  produce NaNs in f16 and run in f32; embedders fail on non-finite output, because non-finite
  vectors made usearch's insert loop forever.
- **Licences:** defaults are permissive; non-commercial models can be configured but are never
  defaults.
- **Not measured:** BGE-M3 (no safetensors weights), snowflake-arctic-embed-m-v2.0 (model code
  candle lacks), 8B models, Nemotron-Embed 1B, nomic-embed-code and Qodo-Embed (far beyond the
  indexing budget), hosted APIs (keys and per-index cost), CPU numbers on Linux (only this Mac's
  CPU).

Changing the default model leaves existing indexes stale until `lode index --force`.

## Reranker

**Current:** off by default. When enabled: gte-reranker-modernbert-base, pool 20, 192-token
pairs.

On the hard set (pool 20):

| Top 8 (search p50) | jina-code-v2, no reranker | jina-code-v2, reranker | Qwen3, no reranker | Qwen3, reranker |
| ---- | ---- | ---- | ---- | ---- |
| Combined | **0.948** (9 ms) | 0.833 (364 ms) | 0.854 (21 ms) | 0.760 (381 ms) |
| Original question | 0.646 | 0.698 | 0.615 | 0.656 |

- It hurts well-phrased queries: −0.115 [−0.188, −0.052] for jina-code-v2 (11 lost, none
  gained). Its 192 tokens cover query and code together, so a two-part query leaves little of
  each candidate, and the hard set is built around look-alikes and behaviour deep in long
  definitions. On the original wording it adds +0.052, not significant.
- On CPU it costs 40×: 65 ms against 2.6 s per search.
- In agent runs (Sonnet, the original pilot tasks) it did not save tool calls: 5.15 against
  4.85 per run, more tokens, +2/54 reuse within noise. It was on by default from the first build
  until the hard set, on the judgement that ~1 s was worth it if it saved agent work; it did not.
- Under concurrency the cross-encoder serializes on the GPU: 8 clients saw p50 7.6 s with it at
  pool 50, 40 ms without.

Rerankers compared (on CodeRankEmbed's 50 candidates, original set):

| Reranker | Licence | Top 8 | Search p50 | vs gte-modernbert |
| ---- | ---- | ---- | ---- | ---- |
| jina-reranker-v3 | CC-BY-NC-4.0 | 0.839 | 5.9 s | +0.058 [+0.015, +0.102] |
| Qwen3-Reranker-0.6B | Apache-2.0 | 0.825 | 3.9 s | +0.044 [+0.000, +0.095] |
| **gte-reranker-modernbert-base** | Apache-2.0 | 0.781 | 1.0 s | — |
| (no reranker) | — | 0.701 | 7 ms | −0.080 |
| jina-reranker-v2-base-multilingual | CC-BY-NC-4.0 | 0.642 | 0.5 s | −0.139 |
| bge-reranker-v2-m3 | Apache-2.0 | 0.613 | 2.0 s | −0.168 |
| granite-embedding-reranker-english-r2 | Apache-2.0 | 0.606 | 1.1 s | −0.175 |
| ms-marco-MiniLM-L6-v2 | Apache-2.0 | 0.496 | 0.3 s | −0.285 |

- The order holds for every embedder (no pairing effect). Qwen3-Reranker gains +0.051 on Qwen3's
  candidates at 2.6× gte's latency.
- **Pool 20:** with a strong embedder, 20 matches 50 (+0.007 [−0.029, +0.044]) at 0.43 s
  instead of 1.0 s; above 50 it gets slightly worse. Weak embedders (CodeRankEmbed) need 50.
- **Token budget:** 128 → 0.65 rank@5, 192 → 0.75, 256 → 0.75 at twice the latency, 512 → 0.70.
  384 tokens with combined queries adds 0 to +0.029 at 1.9× the latency.
- candle's classification head ends in a softmax, constant for one logit, so lodestone applies
  the head itself.

## Queries and the tool surface

**Current:** two search tools, `semantic_code_search(query, example_identifiers)` and
`similar_code_search(code)`, plus `status`; 24 results; combined queries.

**Query wording is the largest lever measured,** larger than any model or setting. Original set,
top 8 without / with the reranker:

| Style | CodeRankEmbed | Qwen3-Embedding-0.6B | jina-code-v2 |
| ---- | ---- | ---- | ---- |
| original question | 0.701 / 0.737 | 0.825 / 0.869 | 0.715 / 0.759 |
| task sentence | 0.883 / 0.891 | 0.942 / 0.934 | 0.869 / 0.898 |
| guessed identifiers | 0.949 / 0.978 | 0.949 / 0.978 | 0.927 / 0.978 |
| combined (sentence, then names) | 0.956 / 0.964 | 0.978 / 0.971 | 0.985 / 0.971 |
| combined, answer names removed | 0.934 / 0.912 | 0.956 / 0.934 | 0.956 / 0.942 |

- Combined is the most robust: removing the answer names costs identifier queries 0.05–0.11 but
  combined only 0.01–0.05. Guessed names help even when wrong.
- **Memorisation inflates these scores.** Rewriters told a public repository's name recall its
  real identifiers: on the hard set they named an answer in 69 of 96 questions, against 5 of 96
  when told the repository was private. Measure on invented names (the hard set's rewrites).
- `semantic_code_search` takes the sentence and the names separately and joins them into the
  combined form, so agents can't drift from it. Its description used to say "not a guessed
  name"; it now asks for both.
- **`similar_code_search`** exists because advice alone did not get agents to search with code
  (0 of 7 searches in a sample, even after reading the skill). It strips diff `+` prefixes and
  lists definitions matching the passed code apart from the ranked results, ignoring whitespace
  and comments (an agent pasted a function without its comments and got itself back). They are
  listed, not left out, since a match may be an existing copy of code not yet indexed. `code` is a list: one call per diff,
  not one per function, after a Codex session made 13 calls returning ~46k characters it never
  read.
- **24 results, no `top_k` on MCP.** Replaying 78 real agent queries: 8 results put the answer in
  view for 59, 16 for 62, 24 for 67. 24 costs about 1,000 tokens per search; 16 would save about
  330, 0.5% of a median run, and lose 5 hits. Given `top_k`, agents asked for 8–12 on every
  `similar_code_search` call, so it was removed.
- **Tool metadata:** both search tools set `_meta["anthropic/alwaysLoad"]`; with them deferred,
  Claude never loaded them in practice. All three are `readOnlyHint`. When embedding is local,
  the descriptions say nothing leaves the machine: Codex's approval reviewer rejected searches
  as sending code "to an external service" before that.

## Search settings

**Current:** vector search only (`lexical_weight = 0`, `identifier_lexical_weight = 0`), tests
filtered out unless asked for, no doc vectors, filters applied inside both index searches.

- **BM25 fusion hurts:** equal-weight fusion took rank@5 from 0.55 to 0.30 in the first spike;
  on the original set 0.25 is neutral and 0.5–1 cost up to −0.21 top 8. Long natural-language
  queries make BM25 noise. The lexical index is still built, so fusion can be re-enabled in
  config.
- **Identifier boosting** (exact name first, BM25 ×2 for identifier-like queries): no gain on
  either repository; `exact_identifier_first` cost 0.03–0.05 top 8.
- **Tests:** filtering them beats down-weighting (rank@5 0.75 against 0.70 at weights 0.5–1.0).
- **Rejected:** module-level chunks (no gain, +3% items), sentence-level query vectors (no
  gain).
- **Doc vectors** (a second vector of each item's doc comment) help CodeRankEmbed (+0.044) but
  hurt Qwen3 (−0.029) and cost 26–33% more embeddings. Summed fusion without a reranker pushes
  undocumented first-ranked answers into the 20s, so they fuse only while the reranker is on.
  Off by default.
- **Filters run inside the searches** (usearch `filtered_search`, tantivy `FilterCollector`).
  Filtering after fusion, on 200 candidates per list, left narrow scopes in large repositories
  with few or no results.
- **Qwen3 query instruction:** the default ("Given a description of what some code should do…")
  beat a generic code instruction (−0.007), an agent-oriented one (−0.029) and none (−0.109).

## Chunking

**Current:** one item per definition from each grammar's `tags.scm` (plus lodestone's
overlays); text is a header line, the doc comment and the source, truncated (not split) at
`chunk_tokens = 200`. Files with no grammar or no definitions are chunked as prose.

| Top 8, no reranker / reranker | 128 | **200** | 350 | 500 |
| ---- | ---- | ---- | ---- | ---- |
| CodeRankEmbed | 0.693 / 0.759 | 0.701 / 0.737 | 0.745 / 0.766 | 0.737 / 0.759 |
| Qwen3-Embedding-0.6B | 0.803 / 0.832 | 0.825 / 0.869 | 0.847 / 0.861 | 0.847 / 0.861 |
| granite-small-r2 | 0.642 / 0.730 | 0.679 / 0.745 | 0.686 / 0.759 | 0.701 / 0.745 |

Only CodeRankEmbed gains significantly (+0.044 at 350), and larger chunks cost indexing time:
350 tokens is 1.26–1.46× and 500 is 1.5–1.86× the time of 200. Not yet measured with
jina-code-v2 on the hard set, where H2 (behaviour past the embedded text) is its weakest
criterion.

- **Lexical tokenizer** splits `snake_case` and `CamelCase` and keeps the whole identifier:
  identifier-query top-1 0.833 against 0.667 for tantivy's default.
- **Overlays are prepended:** tree-sitter-tags keeps the lowest pattern index per name node, so
  an overlay must come first to replace a grammar's undocumented pattern.
- **Own tags queries** for C, C++, C#, Elixir and Swift: upstream C/C++ tag only a function's
  declarator, C#'s does not compile under tree-sitter-tags, Elixir's tags documented modules
  twice, Swift's tags a whole class as each of its methods.
- **Re-chunking is fingerprinted per extension** (its language's queries, text rule or component
  languages), plus a chunker fingerprint (item format, chunk size) that re-chunks everything.
  Embeddings of unchanged text come from the cache. Grammar crate upgrades are not detected.
- **`.h` headers** are C unless they use C++-only syntax: config is global, so one mapping can't
  serve C and C++ projects in the same daemon.
- **Svelte:** `tree-sitter-svelte-ng` only locates `<script>`/`<style>` blocks; scripts are
  tagged as TypeScript or JavaScript, markup is text, and each part is the file with everything
  else blanked so lines and columns stay the file's own. `tree-sitter-htmlx-svelte` (Svelte 5)
  needs tree-sitter 0.26, and only one tree-sitter runtime can link.
- Text paragraphs over the budget are cut at line boundaries, not truncated, for markup with few
  blank lines.

## Output format

**Current:** `path:line:col  lines start-end  signature  — doc`, one line per item.

- `line:col` is the name's position, compiler style, so editors and language servers can jump to
  it; `start-end` lets an agent read exactly the item. Checked against rust-analyzer: positions
  resolve on the first call without a file read.
- All 1-based, matching grep, git, compilers and the Read tool; columns count characters (LSP
  counts UTF-16 units). No hyphen after the column, because GNU-style tools read `line:col-n` as
  a column range.
- Ranges vary a lot by kind (stacks-core, p95 lines): method 64, function 91, impl 391, so
  reading whole impls is not cheap.

## Daemon, freshness and worktrees

**Current:** one daemon per user on a Unix socket, started on demand by `lode mcp`, the hooks and
the CLI, exiting `idle_timeout_secs` after the last lease.

- **Unix socket, mode 0600, TCP opt-in:** no other local user or web page (DNS rebinding) can
  reach the unauthenticated API, and users sharing a machine don't contend for a port. Windows
  keeps TCP on 4600. A lock file next to the socket allows one daemon; it is taken before the
  model loads, so a racing second daemon exits at once.
- **On demand, with leases:** `lode mcp` leases its session's worktree (renewed every 30 s);
  CLI and REST requests take a 10-minute lease; tool calls naming another worktree keep it for
  the session. Only leased worktrees are watched. The idle exit waits for running index passes.
- **The CLI starts the daemon** instead of running in-process, so a search doesn't load the model
  each time. `status` never starts one.
- **Searches activate the worktree they name** (watched, caught up, indexed if new). An MCP
  search in a never-indexed worktree waits up to 30 s for its first index: new agent worktrees
  otherwise answered "still being indexed" to their first searches. An empty answer from an
  index still building says so rather than "no matches", which agents read as "nothing exists".
- **Worktrees never borrow another checkout's index:** placement comes from git (`.git` files,
  `commondir`). Linked worktrees of a registered repository register themselves and index mostly
  from the shared cache; an unregistered checkout gets an error, not the enclosing checkout's
  results.
- **Watched changes go through the full scan's walker,** so nested and global ignore rules apply
  to both. Global git excludes match relative to the checkout being indexed, as in git: matched
  against the daemon's working directory, a rule like `**/.worktrees/**` left a nested worktree
  indexed as empty.
- **Shutdown** finishes in-flight requests but not a running index pass; the manifest is written
  last per batch, so the next pass carries on. SIGTERM behaves like Ctrl-C.
- **usearch thread slots:** each index reserves one per core and further searches wait; usearch
  otherwise fails a search when all slots are taken.

Measured with an earlier model (CodeRankEmbed, no reranker), still the reference for regressions:

| Scenario | Measured |
| ---- | ---- |
| Cold index of stacks-core, Metal | 245 s |
| Re-index with no changes | 0.02 s |
| Second worktree of the same commit | 18.6 s, 0 embedded |
| 8 clients × 200 searches during a reindex | 0 errors, p50 40 ms, p99 43 ms |
| Edit → searchable | 0.7 s |
| Branch switch 15 commits back | 5.4 s, 95.4% cache hits |
| Daemon killed mid-index | restart reports stale, reuses cached vectors, consistent index |

## Agent integration

**Current:** `lode install` adds a skill, the MCP server and reminder hooks (SessionStart,
UserPromptSubmit and SubagentStart for Claude Code; SessionStart for Codex), plus a PreToolUse
nudge on Claude's shell commands and a hint naming existing code like what its file edits add.
The reminder leads with general advice to find and reuse existing code, then points at
lodestone, and fires only when a daemon serves the session's repository (or folder).

**Opus 5.5, 28 tasks on stacks-core and LibreChat, 48 positive runs per arm**
([details](../eval/situations/README.md#situational-run-opus-55-stacks-core-and-librechat-sit-json)):

| Arm | Correct | Cost per correct answer |
| ---- | ---- | ---- |
| No reminder | 27/48 | $0.296 |
| Advice only (the same reminder text, no lodestone) | 37/48 | $0.227 |
| Advice + lodestone (plugin) | **43/48** | $0.194 |

Plugin against advice only: +0.125 per task [+0.000, +0.271], +0.208 [+0.042, +0.417] on
LibreChat, which Claude knows less well; wall time ×0.93 [×0.89, ×0.98]. On a private 185k-line
Rust workspace, correctness was at the ceiling in both arms, but lodestone found the answer with
half the tool calls and ×0.67 the tokens. Most of the gain over nothing is the advice; lodestone
adds the most where guessing names fails.

Why it is shaped this way (Sonnet, stacks-core, 624 runs unless noted):

- **The skill alone does nothing:** it was never loaded in any of 576 runs, with any description.
  Its listing text still steers agents: request-matching wording ("Use when asked to write, add or
  implement a function, … or to review, refactor or deduplicate code") made them search in 48% of
  write runs and lifted correctness 24 → 36/54 (+0.204 [+0.056, +0.389]); "Use before writing…"
  reached 6%.
- **The reminder is what gets agents searching:** write runs searched 50% with it against 6%
  without; review 69% against 31%, refactor 97% against 43%. Locate tasks, where grep suffices,
  stayed low.
- **On stacks-core, advice alone did as well on write tasks** (37/54 either way): its helper
  names are predictable and partly memorised. That is why later tasks target unfamiliar code and
  names that can't be guessed.
- **The model matters more than the wording:** Sonnet at medium effort searched in 2 of 9 pilot
  runs, at high in 5, Opus 5.5 in 9.
- **Harness gaps found from transcripts:** the desktop app skips SessionStart at startup (hence
  UserPromptSubmit, once per session via a marker in `~/.lodestone/hook-sessions/`); it starts
  MCP servers in the main checkout of a worktree session (hence the marker records the session's
  directory, and `lode mcp` uses it); subagents get none of the session's reminders (hence
  SubagentStart); sessions often start in a folder of checkouts (hence the folder variant).
- **Side effect:** earlier reminder wording crowded out stacks-core's own `stacks-pr-review`
  skill (loaded in 3–9 of 36 review runs instead of 31). With the advice-first wording it loads
  in 8 of 8 on Opus.
- **Install layout:** the skill lives in `.agents/skills/lodestone/`, which Codex reads at both
  scopes; Claude Code doesn't read a repository's `.agents/skills`, so it gets a symlink from
  `.claude/skills/`. Codex's auto-approver denied lodestone calls, so install sets
  `default_tools_approval_mode = "approve"`, editing `config.toml` directly because
  `codex mcp add` can't.
- **A nudge at the grep:** with the tools loaded and every session and subagent reminded, Claude
  still made about one search per 120 tree greps; a reminder at session start is far from the
  moment it greps. `lode hook pre-tool-use` adds one line pointing at `semantic_code_search`
  when a shell command searches a served tree for three or more alternative names, and is silent
  otherwise. It never blocks: an older `lode` answers an unknown hook with exit status 2, which
  Claude Code reads as "block the call", so the installed command ends in `|| true` and `lode
  hook` exits 0 for hooks it doesn't know, with no daemon, or when the daemon is slow (0.1 s).
- **An edit hint:** only 4 of 22 Claude sessions that edited files ran `similar_code_search`
  after an edit. The `PreToolUse` hook also runs before `Edit`, `MultiEdit` and `Write`, chunks
  the new text and searches for each definition it adds; when an existing one in the same language
  is close enough, it names up to three in one line. Codex isn't covered: its edits come as
  `apply_patch` patches. The cut-off (`[hooks] duplicate_similarity`) was measured on this
  repository with jina-code-v2, using raw definition source as the query, as
  `similar_code_search` does:

  | Query | Top non-self similarity |
  | ---- | ---- |
  | Exact copy of an existing function | 0.94 |
  | 17 renamed, restyled rewrites of existing helpers | 0.72–0.87 (median 0.83) |
  | 28 new definitions with no existing counterpart (same-domain hooks, configs, parsers, boilerplate) | 0.39–0.71 (median 0.59) |

  0.75 catches 14 of the 17 rewrites with no false positive; 0.72 catches 16 but leaves a 0.01
  margin over boilerplate (`impl Default`, one-line path getters, `match` tables). Prefixing the
  indexed chunk header (`path kind name`) narrowed the gap (rewrites from 0.74, unrelated to
  0.69), so the query is the code alone. Latency of the release build against a warm daemon:
  70 ms for an edit adding no definition (loading only the file's grammar; all grammars take
  1.3 s), 90 ms for one definition, 200 ms for 12 and 330 ms for 24; 70 ms with no daemon.
- **Install touches only its own hooks:** `lode install` and `uninstall` replace only the hook
  command they wrote (or one written from another path to `lode`), so a user's hook that runs
  `lode` stays.

**Eval harness caveats:** `claude -p --setting-sources project` still loads `~/.claude/CLAUDE.md`
(`run.py` now excludes it); parallel batches must not share an MCP config file.

## Usage statistics

- Calls count towards the registered repository they worked in (file path or command directory,
  else `repo`, else the session's directory), with linked worktrees under their repository, also
  when named as `repo`: attributing by the session's starting directory missed most Codex work,
  which happens in Codex worktrees. Calls outside every registered repository are dropped; in one
  session about 890 of 990 shell greps ran over scratch data.
- Shell commands are read by `shell::analyse`, shared with the nudge: the first pipeline stages,
  following `cd` and `NAME=value`, for searches and file reads. A grep counts only when it
  searches files; about a quarter of the commands counted before only filtered build or test
  output. Shell reads (`cat`, `sed -n`, `head`) count as reads: Claude made 875 of them against
  77 `Read` calls in four sessions, so its searches had shown 0% "with a result read".
  `eval/situations/score.py` mirrors the statement splitting; both check
  `tests/fixtures/shell-searches.json`.
- Reading is incremental (offset, size, mtime and a hash of each transcript's first 4 KB), and
  extracted calls outlive the harnesses' own transcript cleanup. A parser version bump or a new
  registration re-reads only the transcripts still on disk, replacing each one's calls in the
  transaction that reads it, so data from deleted transcripts and from the other harness stays.

## Packaging

- **Crate name `lodestone-cli`:** `lodestone` and `lode` are taken on crates.io; the library is
  `lodestone` and the binary `lode`.
- **Release profile:** symbols stripped, fat LTO, one codegen unit. With fastembed opt-in, the
  default `lode` went from 95.0 MB to 48.0 MB; with `--features fastembed` it is 67.5 MB. Panic
  backtraces lose function names. Under LTO, ONNX Runtime needs clang's runtime on macOS, which
  `build.rs` links.

| Change | Size | Stripped |
| ---- | ---- | ---- |
| `strip = true` | −22.2 MB | — |
| fastembed opt-in | −28.6 MB | −19.7 MB |
| `lto = "fat"`, `codegen-units = 1` (`thin`: +0.4 MB) | −13.1 MB | −5.4 MB |

- **All 16 grammars are on by default.** Parse tables are most of their cost (unstripped, before
  LTO): C# 5.2 MB, Swift 4.0, C++ 3.4, Ruby 2.1, PHP 2.0, Elixir 1.3, C 0.6, Java 0.4, Lua 0.05;
  the original five added 4.4 MB together.

## History

The first plan set acceptance targets on the 24-question stacks-core pilot set: rank@5 ≥ 0.85
(never met: best 0.75, with the reranker), all four "hard" helpers (RetryReader,
bounded_format, guarded_string, select_transactions_where) in the top 5 (met only with the
reranker), cold index under 10 minutes and sub-second search (met). The four questions no
configuration answered described domain behaviour in words shared with neither the code nor its
docs. The pilot set was too small to rank models, which led to the 137-question set, and that set
saturating led to the hard set.

The first agent experiment (Sonnet, 18 write tasks × 3) forced a `search` call by system prompt:
reuse rose from 40/54 (reuse rule alone) to 44/54. Its misses were about query framing: agents
searched for their task's nouns rather than the capability they needed, which no retrieval change
fixes. That led to measuring unprompted use, and to the reminder.
