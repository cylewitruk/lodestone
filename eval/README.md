# Evaluation sets

Question sets and tasks for [docs/evaluation.md](../docs/evaluation.md), which says how to fetch
each corpus and run them. The corpora are examples; lodestone must not depend on or special-case
any of them.

## Files

| File | Contents |
| ---- | ---- |
| `questions.json` | 74 questions against stacks-core `95c6a45eed`: `easy` (12), `mismatch` (52), `negative` (10). q01–q24 are from the original reuse pilot; sa/sb were added for the model comparison |
| `questions-tantivy.json` | 44 questions against tantivy 0.27.0: 5 easy, 34 mismatch, 5 negative |
| `questions-librechat.json` | 40 questions against LibreChat `0e719592` (TypeScript/JavaScript): 6 easy, 28 mismatch, 6 negative |
| `questions-hard-*.json` | 96 harder questions, 32 per repository (below) |
| `variants/` | query rewrites per style, and `build.py`, which builds per-style question files |
| `compare.py` | pooled comparison of `lode bench --json` reports, paired against a baseline |
| `tasks.json` | 18 stacks-core write tasks; `accept` is a regex the reply's code must match to count as reuse. Used by `situations/` |
| `situations/` | the agent experiment ([README](situations/README.md)) |

Each question is `{id, kind, question, answers: [{path, symbol}]}`; `lode bench` scores the rank
of the first expected answer. tantivy's t13 was originally a negative; `parse_into_milliseconds`
parses exactly the strings it describes, so it is now answerable.

## How the questions were written

Agents that read the code drafted the questions under these rules: describe the need as a
developer who has not seen the code, avoid the identifier and its doc-comment wording, list every
definition that genuinely answers, and verify negatives with several greps for synonyms. Every
answer was checked to be an item the chunker extracts. Because the writers read doc comments,
questions may lean slightly towards documented items.

## Hard set

`questions-hard-{stacks,tantivy,librechat}.json` exist because the sets above saturate: with
well-phrased queries every model put the answer in its top 8 for 95–98% of them. Each question
meets at least two of these criteria, recorded in `criteria`:

| Criterion | Meaning |
| ---- | ---- |
| H1 | the answer has no doc comment |
| H2 | the behaviour asked about lies past the ~800 characters of a definition that are embedded |
| H3 | at least two look-alike definitions exist (listed in `near_misses`); the question turns on the detail that separates them |
| H4 | the question uses the words of a developer with a problem, not the code's names, comments or strings |

Writers chose targets with grep and file reads only, never a search tool, so no question was
selected because some model fails it. A separate reviewer re-derived each question's answers,
adding other valid answers (`review: "answers added"`) or rewording to rule out look-alikes
(`review: "reworded"`).

Query rewrites (`variants/hard-*.json`) were written from the question text alone by writers told
the repository was private and to invent every name. A first round, told the repository's name,
recalled real identifiers from training data and named an answer in 69 of 96 questions, against 5
of 96 without; it is kept as `variants/hard-recall-*.json`. Popular open-source repositories are
partly memorised, so rewrites that may draw on that memory overstate how search behaves on
unfamiliar code.
