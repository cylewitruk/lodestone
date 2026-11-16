# Evaluation

## Retrieval

Fetch the example corpus, then measure:

```bash
git clone https://github.com/stacks-network/stacks-core eval/corpus && git -C eval/corpus checkout 95c6a45eed
lode bench eval --hard q06,q07,q08,q18
```

| Output | Meaning |
| ---- | ---- |
| First line | share of questions whose answer is in the top 24 results (what an agent sees) |
| rank@1/5/10, MRR | ranking accuracy |

- **Comparing setups:** `--model` and `--backend`; `--rerank`, `--reranker` or
  `--rerank-candidates` for the reranker.
- **Other repositories:** `--corpus` and `--questions`, with questions shaped
  `[{id, kind, question, answers: [{path, symbol}]}]`.

## Agents

Headless Claude Code evaluations live in [eval/harness/](../eval/harness/README-lode.md) and
[eval/situations/](../eval/situations/README.md). Question sets and corpora are described in
[eval/README.md](../eval/README.md).

Results and the decisions they led to are in the [decision log](decision-log.md).
