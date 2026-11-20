# Evaluation

Retrieval is measured with `lode bench` on question sets; agent behaviour with headless Claude
Code runs. Results and the decisions they led to are in the [decision log](decision-log.md); the
question sets themselves are described in [eval/README.md](../eval/README.md).

## Retrieval

Fetch a corpus, then run `lode bench` against its questions:

| Corpus | Fetch | Questions |
| ---- | ---- | ---- |
| stacks-core (Rust) | `git clone https://github.com/stacks-network/stacks-core eval/corpus && git -C eval/corpus checkout 95c6a45eed` | `questions.json`, `questions-hard-stacks.json` |
| tantivy 0.27.0 (Rust) | any copy of the crate source, e.g. Cargo's in `~/.cargo/registry/src/index.crates.io-*/tantivy-0.27.0` | `questions-tantivy.json`, `questions-hard-tantivy.json` |
| LibreChat (TS/JS) | `git clone https://github.com/danny-avila/LibreChat /tmp/librechat && git -C /tmp/librechat checkout 0e719592` | `questions-librechat.json`, `questions-hard-librechat.json` |

```bash
lode bench eval --hard q06,q07,q08,q18                    # stacks-core, eval/questions.json
lode bench eval --corpus /tmp/librechat --questions eval/questions-librechat.json
```

| Output | Meaning |
| ---- | ---- |
| First line | share of questions whose answer is in the top 24 results (what an agent sees) |
| rank@1/5/10, MRR | ranking accuracy |

- **Comparing setups:** `--model` and `--backend`; `--rerank`, `--reranker` or
  `--rerank-candidates` for the reranker. `eval/compare.py` pools `lode bench --json` reports
  across corpora, with paired wins and losses and bootstrap intervals against a baseline.
- **Query styles:** `eval/variants/build.py` builds per-style question files from the rewrites
  in `eval/variants/`:

  ```bash
  python3 eval/variants/build.py /tmp/qv --hard
  lode bench eval --corpus eval/corpus --questions /tmp/qv/hard-stacks-combined.json --no-rerank
  ```

- **Other repositories:** `--corpus` and `--questions`, with questions shaped
  `[{id, kind, question, answers: [{path, symbol}]}]`.

## Agents

[eval/situations/](../eval/situations/README.md) runs headless Claude Code (`claude -p`) on
write, review, refactor and locate tasks, with and without lodestone, and scores correctness,
tool use and cost. Its README has the design, the commands and every result.
