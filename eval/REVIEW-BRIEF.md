# Brief for an outside review: lodestone retrieval and agent-usage experiments

You are asked for a critical second opinion on a set of experiments and the decisions taken from
them. Please look for flaws in method, alternative explanations, decisions the evidence does not
support, and the most useful next experiments. Disagreement is the point.

## Context

lodestone is a local semantic code-search daemon for coding agents (Rust; candle on Metal/CPU,
tree-sitter definitions, usearch HNSW, optional cross-encoder reranker and BM25 fusion). Agents
call one MCP tool, `search`, with a free-text query and get back the 8 best-matching definitions.
The main metric is **top 8**: the share of questions whose answer is among those 8 results.
Comparisons are paired per question with 95% bootstrap intervals (`eval/compare.py`).

Each definition is one vector: header line, doc comment, then its own source, cut at about 800
characters (no splitting, no overlap).

## 1. Retrieval settings sweep (original question set)

137 answerable questions over stacks-core (Rust), tantivy (Rust) and LibreChat (TS/JS), written by
agents that read the code. Six embedders, reranker variants, chunk sizes, fusion, query wording.

- **Query wording dominated everything.** Rewriting the questions into "task" sentences or guessed
  identifiers raised top 8 by 0.10–0.25 for every embedder. A "combined" query (one sentence,
  then guessed names) was the most robust. With it, all models reached 0.94–0.985 and could no
  longer be told apart.
- **Settings left unchanged:** BM25 fusion (any weight ≥ 0.5 hurts, up to −0.21), identifier
  fusion and exact-name-first (no gain), Qwen3 query instructions (the default was best).
- **Chunk size:** only CodeRankEmbed gained significantly (+0.044 at 350 tokens); others ≤ +0.02.
- **Rerankers:** Qwen3-Reranker-0.6B ranked best for every embedder (no pairing effect), at about
  2.6× the latency of the default gte-reranker-modernbert-base.

## 2. Memorisation confound

Rewrites were written "blind" (question text only) by Claude agents. On public repositories the
writers recalled real identifiers from training data: in a later test, rewriters told the
repository's name named an answer symbol in 69 of 96 questions, against 5 of 96 when told the
repository was private and to invent names. The original set's LibreChat identifier rewrites
named the answer 30 of 34 times. So identifier-style scores on public code overstate what an
agent would get on unfamiliar code.

## 3. Hard question set

96 questions (32 per repository), each meeting at least two criteria: answer undocumented (H1),
behaviour past the embedded ~800 characters (H2), look-alike definitions exist (H3), vocabulary
gap (H4). Writers used grep and file reads only (no search tool, so no model-dependent
selection); a separate reviewer re-derived every answer list. Combined rewrites were made by
writers told to invent names.

| Embedder (top 8, no reranker) | Original wording | Combined | Combined, recalled names |
| ---- | ---- | ---- | ---- |
| jina-embeddings-v2-base-code | 0.646 | **0.948** | 0.969 |
| Qwen3-Embedding-0.6B | 0.615 | 0.854 | 0.948 |
| CodeRankEmbed | 0.542 | 0.844 | 0.948 |
| granite-embedding-english-r2 | 0.479 | 0.750 | 0.927 |
| UltraBERT-Embed-150M | 0.427 | 0.708 | 0.896 |
| granite-small-r2 | 0.396 | 0.698 | 0.896 |

jina-code-v2 against Qwen3 on combined: +0.094 [+0.042, +0.156], 9 questions gained, 0 lost,
ahead in all three repositories and under every criterion. On the original set Qwen3 had led
jina-code-v2 by 0.11 on original wording; on the hard set they are level.

**Reranker on the hard set (pool 20, 192-token budget):**

| Top 8 | jina-code-v2 | Qwen3 |
| ---- | ---- | ---- |
| combined, no reranker | 0.948 | 0.854 |
| combined, reranker | 0.833 (−0.115 [−0.188, −0.052]) | 0.760 (−0.094) |
| original, no reranker | 0.646 | 0.615 |
| original, reranker | 0.698 (+0.052, n.s.) | 0.656 (+0.042, n.s.) |

Our explanation: the budget covers query and code together, so a long combined query leaves the
cross-encoder little of each candidate; the set is also built around look-alikes and behaviour
deep in long definitions. Latency: about 9 ms without the reranker against 380 ms with it on
Metal; on CPU 65 ms against 2.6 s.

**Decisions taken:** default embedder jina-code-v2 on every backend (Metal, CPU, fastembed),
reranker off by default, and the `search` tool description now asks for combined queries (it
previously said "not a guessed name").

## 4. Agent situations experiment

Headless Claude Code (Sonnet, `claude -p`, tools Read/Grep/Glob/Skill, user settings excluded) on
stacks-core, 3 reps per task. Four situations: **write** (18 tasks: write a helper; correct if it
calls the existing one), **review** (12 diffs, 8 quietly re-implement an existing helper, 4
controls), **refactor** (10: name the existing duplicate of a given function, 2 controls),
**locate** (12: find the function behind a bug report; 6 quote a literal log line). Arms: `none`
(no lodestone), `tools` (search over MCP), `skill` (plus a lodestone skill), `hook` (plus a
SessionStart reminder injected by the plugin). No system-prompt rule anywhere.

| Situation | Correct: none / tools / skill / hook | Runs that searched: tools / skill / hook | Cost per correct: none → hook |
| ---- | ---- | ---- | ---- |
| write | 25 / 25 / 24 / **37** of 54 | 4% / 6% / 50% | $0.100 → $0.071 |
| review | 33 / 34 / 34 / 33 of 36 | 25% / 31% / 69% | $0.078 → $0.045 |
| refactor | 30 / 30 / 30 / 30 of 30 | 17% / 43% / 97% | $0.059 → $0.045 |
| locate | 36 / 36 / 36 / 36 of 36 | 6% / 8% / 3% | $0.038 → $0.036 |

- Write, hook vs none: +0.222 per task [+0.093, +0.370]; runs that searched were correct 22/27,
  runs that did not 15/27. Every search used the combined format.
- Review, refactor and locate are at the ceiling in every arm (grep plus Claude's knowledge of
  stacks-core suffices), so they show adoption and cost only.
- **The lodestone skill was never loaded** in any of the 576 runs where it was installed, with
  any description.

**Skill descriptions** (write, no reminder, 54 runs each): "Find existing code … Use before
writing…" → 6% searched, 24/54; "Use before writing any new function…" → 9%, 30/54; same with a
task-shaped name → 9%, 28/54; **"Use when asked to write, add or implement a function, … or to
review, refactor or deduplicate code"** → 48%, 36/54 (+0.204 [+0.056, +0.389]). A probe showed
the agent sees each description verbatim. Our reading: the listing text acts as an instruction;
request-matching wording works, precondition wording does not. Without the reminder, that
description searched less in review (44% vs 69%) and refactor (77% vs 97%), so the shipped plugin
keeps both, and the reminder only fires when a running daemon serves the repository.

**Side effect:** stacks-core ships its own `stacks-pr-review` skill. It loaded in 31/36 review
runs without lodestone, 27 with the tool alone, but 3–9 once lodestone's text mentioned
reviewing. With the original description, 21/36 runs neither loaded it nor searched. Our grader
only checks duplicate detection, so the cost of skipping the repository's review process is not
measured. Not yet fixed.

## Known weaknesses

- All questions, rewrites and tasks were written by Claude agents; the agents under test are
  Claude too.
- One agent model (Sonnet); agent tests on one public, likely memorised repository.
- Hard set n = 96, intervals ±0.05–0.10; about 50 comparisons in the sweep, so a few narrow
  "significant" results are expected by chance. No held-out set.
- Regex graders; review/refactor/locate saturate; timings are per machine (M4 Max).
- The rewrite format (task sentence + identifiers) was invented by us and then used as the target
  behaviour, which may favour models that happen to like it.

## Questions

1. Is the hard set's construction sound, or could its criteria (for example undocumented answers,
   look-alikes) systematically favour jina-code-v2 or code-trained encoders?
2. Is switching the default to jina-code-v2 with the reranker off justified by this evidence?
   What would you need to see first?
3. Is the 192-token budget the right explanation for the reranker hurting combined queries? What
   alternatives would you test (reranking with the task sentence only, larger budgets, a
   different reranker)?
4. How would you measure retrieval on unfamiliar code without memorisation (identifier
   obfuscation, private repositories, newer repositories)?
5. Are the agent situations and graders realistic? How would you get review, refactor and locate
   off the ceiling?
6. Do you agree with the interpretation of the skill results (never loaded, description text
   steering behaviour)? Is a SessionStart reminder an acceptable mechanism, and how should
   lodestone avoid displacing repositories' own review skills?
7. What important experiment or failure mode is missing?

## Files

| Path | Contents |
| ---- | ---- |
| `CHANGELOG.md` | all results and decisions; sections "Model comparison", "Settings sweep", "Hard question set", "Agent situations and the lodestone plugin" |
| `eval/README.md` | question sets, hard-set method |
| `eval/questions-hard-*.json`, `eval/variants/` | hard set (criteria, near misses, review notes) and rewrites, incl. `hard-recall-*` |
| `eval/compare.py`, `eval/variants/build.py` | paired comparison; per-style question files |
| `eval/situations/` | agent experiment: `README.md` (design, results), task sets, `run.py`, `score.py`, skill variants |
| `plugin/` | shipped Claude Code plugin: skill, MCP config, SessionStart hook |
| `src/config.rs`, `src/api/mcp.rs` | defaults and the `search` tool description |
