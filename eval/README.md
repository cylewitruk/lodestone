# Evaluation assets

Retrieval and agent-level evaluation sets from the 2026-11 stacks-core reuse pilot. See `../PLAN.md` §9.

## Corpus

`stacks-core` is the *example* repository these sets were written against; lodestone itself must
not depend on or special-case it. All answers reference commit `95c6a45eed` (main, 2026-11-06):

```bash
git clone https://github.com/stacks-network/stacks-core eval/corpus && git -C eval/corpus checkout 95c6a45eed
```

## Files

| File | Purpose |
| ---- | ---- |
| `questions.json` | 74 retrieval questions against stacks-core: `easy` (12), `mismatch` (52), `negative` (10). `answers[].path`/`symbol` are the expected hits; `lode bench` scores rank@k against them. q01–q24 are the pilot's; sa/sb were added for the model comparison |
| `questions-tantivy.json` | 44 questions against tantivy 0.27.0 (see below) |
| `questions-librechat.json` | 40 questions against LibreChat, a TypeScript/JavaScript repository (see below) |
| `questions-hard-*.json` | 96 harder questions, 32 per repository (see below) |
| `variants/` | query rewrites per style and `build.py`, which builds per-style question files |
| `compare.py` | pooled comparison of `lode bench --json` reports across the three corpora, with paired wins/losses and bootstrap intervals against a baseline |
| `tasks.json` | 18 implementation tasks; `accept` is a regex the reply's Rust code must match to count as reuse |
| `harness/run_trials.py` | headless `claude -p` runner, one run per (arm, task, rep); add a `rule_lode` arm pointing `--mcp-config` at the daemon |
| `harness/score.py` | grader for both files |
| `harness/reuse-rule.md` | the system-prompt rule used by every non-stock arm |
| `harness/gen_*.py` | static catalogue generators (reference heuristics for item extraction and crate-graph parsing) |
| `harness/ck_mcp.py` | minimal stdio MCP probe client |

## Pilot results to beat (Sonnet, 18 tasks × 3 reps)

| Arm | Reuse | Hard cases (RetryReader, bounded_format, guarded_string, select_transactions_where) | Median cost/run |
| ---- | ---- | ---- | ---- |
| stock | 23/54 | 0/4 | $0.031 |
| reuse rule | 38/54 | 0/4 | $0.039 |
| rule + compact catalogue in prompt | 46/54 | 3/4 | $0.042 |
| rule + reuse skill (per-crate slices) | 42/54 | 3/4 | $0.156 |
| rule + `ck` forced (serial) | n/a | 1/4 | $0.064 |

## Second repository (overfitting check)

`questions-tantivy.json` holds 44 questions (5 easy, 34 mismatch, 5 negative) written against the
`tantivy` 0.27.0 crate source, used to check that ranking changes generalize beyond stacks-core.
t13 was originally a negative; `parse_into_milliseconds` parses exactly the strings it describes,
so it is now answerable. Any copy of the crate source works as the corpus, e.g. the one Cargo already
downloaded:

```bash
lode bench eval --corpus ~/.cargo/registry/src/index.crates.io-*/tantivy-0.27.0 --questions eval/questions-tantivy.json
```

## Third repository (non-Rust)

`questions-librechat.json` holds 40 questions (6 easy, 28 mismatch, 6 negative) against
[LibreChat](https://github.com/danny-avila/LibreChat) at commit `0e719592`, a TypeScript and
JavaScript monorepo (api/, client/, packages/). It checks that results hold outside Rust.

```bash
git clone https://github.com/danny-avila/LibreChat /tmp/librechat && git -C /tmp/librechat checkout 0e719592
lode bench eval --corpus /tmp/librechat --questions eval/questions-librechat.json
```

## Writing questions

Questions added for the model comparison were drafted by agents that read the code, under these
rules: describe the need as a developer who has not seen the code, avoid the identifier and its
doc-comment wording, list every definition that genuinely answers, and verify negatives with
several greps for synonyms. Every answer was then checked to be an item the chunker actually
extracts (path and name). Because the writers read doc comments, questions may lean slightly
towards documented items.

## Hard set

`questions-hard-{stacks,tantivy,librechat}.json` hold 96 answerable questions written because the
sets above saturate: with well-phrased queries every model put the answer in its top 8 for 95–98%
of them. Each question meets at least two of these criteria, recorded in `criteria`:

| Criterion | Meaning |
| ---- | ---- |
| H1 | the answer has no doc comment |
| H2 | the behaviour asked about lies past the ~800 characters of a definition that are embedded |
| H3 | at least two look-alike definitions exist (listed in `near_misses`); the question turns on the detail that separates them |
| H4 | the question uses the words of a developer with a problem, not the code's names, comments or strings |

Writers chose targets with grep and file reads only, never a search tool, so no question was
selected because some model fails it. A separate reviewer then re-derived each question's answers
from the code, adding other valid answers (`review: "answers added"`) or rewording to rule out
look-alikes (`review: "reworded"`).

Query rewrites (`variants/hard-*.json`) were written from the question text alone by writers told
the repository was private and to invent every name. A first round, told the repository's name,
recalled real identifiers from training data and named an answer in 69 of 96 questions, against 5
of 96 without; it is kept as `variants/hard-recall-*.json`. Popular open-source repositories are
partly memorised, so rewrites that may draw on that memory overstate how search behaves on
unfamiliar code.

```bash
python3 eval/variants/build.py /tmp/qv --hard
lode bench eval --corpus eval/corpus --questions /tmp/qv/hard-stacks-combined.json --no-rerank
```
