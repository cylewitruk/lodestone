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
| `questions.json` | 24 retrieval questions: `easy` (4), `mismatch` (16), `negative` (4). `answers[].path`/`symbol` are the expected hits; `lode bench` scores rank@k against them |
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

`questions-tantivy.json` holds 14 questions (1 easy, 11 mismatch, 2 negative) written against the
`tantivy` 0.27.0 crate source, used in Phase 3 to check that ranking changes generalize beyond
stacks-core. Any copy of the crate source works as the corpus, e.g. the one Cargo already
downloaded:

```bash
lode bench eval --corpus ~/.cargo/registry/src/index.crates.io-*/tantivy-0.27.0 --questions eval/questions-tantivy.json
```
