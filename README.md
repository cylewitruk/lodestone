# lodestone

Local, centralized semantic code search for coding agents. One daemon per machine indexes any
number of repositories and worktrees, keeps them fresh from file events, and answers "what in
this codebase does X?" over MCP and HTTP in well under a second.

- Definitions come from tree-sitter tags queries (Rust, Python, TypeScript/JavaScript, Go and
  Markdown built in; any other grammar via tree-sitter's own config).
- Retrieval: embeddings (jina-embeddings-v2-base-code on GPU and CPU), one line per item. A
  cross-encoder reranker (`[rerank] enabled`) and a BM25 index fused in with reciprocal rank
  fusion (`[search] lexical_weight`, `identifier_lexical_weight`) are available but off by default.
- Queries work best as one sentence on what the code does, followed by guessed function or type
  names; a wrong guess still helps.
- Content-addressed embedding cache shared by every worktree: a second worktree of the same
  repository costs chunking and hashing only.
- Three MCP tools (`semantic_code_search`, `similar_code_search`, `status`); a REST API for
  scripts.

## Install

Requires a stable Rust toolchain (edition 2024) and a C compiler.

```bash
cargo install --path .
```

On macOS the default build runs models on the GPU through candle and Metal; elsewhere it runs
them on the CPU through candle. fastembed (ONNX Runtime) is an optional backend for models it
ships. Language grammars are features (`lang-rust`, `lang-python`,
`lang-typescript`, `lang-go`, `lang-markdown`; all on by default).

Models download on first use into `~/.lodestone/models`. `lode models` lists the configured
models and backends; `lode languages` lists grammars and the extension map.

## Start the daemon

```bash
lode serve
```

It listens on `127.0.0.1:4600` (change with `--port` or `port` in `~/.lodestone/config.toml`),
keeps the embedding model resident, re-opens every registered worktree, starts a file watcher
per worktree and catches up on changes made while it was down.

## Register a repository

```bash
lode register ~/src/my-repo
```

Registering canonicalizes the path, adds it to `~/.lodestone/registry.json`, starts watching it
and indexes it. Each additional worktree of the same repository is registered the same way and
reuses cached embeddings. Then:

```bash
lode search "retry an HTTP call with exponential backoff"
```

```bash
lode status
```

Every subcommand talks to the daemon when it is running and runs in-process otherwise (pass
`--local` to force in-process). Searches never index: a stale index answers from what exists and
`status` says why.

Files are walked with `.gitignore` and `.lodestoneignore` rules; directories containing
`CACHEDIR.TAG` (such as Cargo's `target/`) are skipped. On Linux, large trees may need a higher
`fs.inotify.max_user_watches`; if the watcher fails, lodestone falls back to a 60 s rescan poll
and reports it in `status`.

## Install into Claude Code and Codex

```bash
lode install                      # every harness whose CLI is on PATH, for all your sessions
lode install --client codex       # one harness; repeat --client for several
lode install --scope project      # files to commit in the current repository instead
lode uninstall                    # same flags; --dry-run prints the steps without applying them
```

`install` adds three things to each harness:

| Part | User scope | Project scope |
| ---- | ---- | ---- |
| Skill (when and how to search) | `~/.agents/skills/lodestone/`, linked from `~/.claude/skills/` | `.agents/skills/lodestone/`, linked from `.claude/skills/` |
| MCP server | `claude mcp add` / `codex mcp add` | `.mcp.json` / `.codex/config.toml` |
| SessionStart reminder | `~/.claude/settings.json` / `~/.codex/hooks.json` | `.claude/settings.json` / `.codex/hooks.json` |

The skill uses the shared `.agents/` location, which Codex reads at both scopes. Claude Code
reads `~/.agents/skills` but not a repository's `.agents/skills`, and documents only
`.claude/skills`, so it gets a symlink. MCP servers and hooks have no shared location.

The reminder runs `lode hook session-start`, which prints it only when the daemon (port 4600, or
`LODESTONE_PORT`) serves the session's repository. Codex asks you to trust a new hook once
(`/hooks`). Project scope needs Codex to trust the project and Claude Code to approve the
`.mcp.json` server on first use.

`plugin/` holds the same skill, server and hook as a Claude Code plugin
(`claude --plugin-dir plugin`); its hook needs `lode` on PATH.

The reminder tells agents to look for existing code and reuse what fits, then points them at
lodestone. In `eval/situations/` (28 realistic tasks on stacks-core and LibreChat, Opus 5.5), the
advice alone raised correctness from 27/48 to 37/48, and the plugin to 43/48 (+0.125 per task
over the advice, [+0.000, +0.271]; +0.21 on LibreChat), with 7% shorter runs and a 15% lower cost
per correct answer. On a private repository the model cannot know, lodestone found answers with
half the tool calls, though both arms solved nearly every task there. Splitting `search` into
`semantic_code_search` and `similar_code_search` with 24 results (one run per task, 27 tasks on all
three repositories) moved correctness from 23/27 to 25/27 with 12% fewer tool calls at about the
same cost once prompt-cache warmth is evened out.

Agents then see three tools:

- `semantic_code_search`: "Find existing code by what it does, when you do not know its name. …"
  with `query` (one sentence) and `example_identifiers` (guessed names), joined into one query.
- `similar_code_search`: "Find existing code that does the same job as a given piece of code. …"
  with `code`, the source of one function or type (diff `+` prefixes are stripped). The passed
  code's own definition is left out of the results, ignoring whitespace and comments.
- `status`: index freshness for a repo.

Both searches take optional `repo`, `from_unit` (layer filter from the build graph), `language`
and `include_tests`, and return 24 results. There is no `top_k`: agents asked for 8 to 12 and
missed answers ranked below that. REST and the CLI keep `top_k` (default 24).

Results are plain text, one line per item:

```text
stacks-common/src/util/retry.rs:25:12  lines 25-29  pub struct RetryReader<'a, R: Read>  — Wrap a Read so that we store a copy of what was read.
```

- `path:25:12` is the name's line and column in compiler style, ready for an editor or a
  language server. Language servers count from 0, so subtract 1 from both.
- `lines 25-29` is the definition's inclusive line range, doc comments above it excluded, so an
  agent can read exactly that item.
- All numbers are 1-based, like grep, sed, compilers and Claude Code's Read tool. Columns count
  characters. JSON results carry `line`, `line_end`, `name_line` and `name_column`.

## REST

```text
GET  /v1/search?q=&repo=&from_unit=&language=&top_k=&include_tests=&with_body=&format=text|json
POST /v1/search   {query, repo, from_unit, language, top_k, include_tests, path_prefix, kinds, with_body}
GET  /v1/status?repo=&format=text|json
POST /v1/register {path}
POST /v1/index    {repo, force}
GET  /v1/repos
```

## Configuration

`~/.lodestone/config.toml` (all keys optional; `LODESTONE_HOME` relocates the data directory):

```toml
port = 4600
model = "jina-code-v2"      # alias into [models]; see `lode models`
backend = "candle-metal"    # candle-metal | candle-cpu | fastembed | remote
batch_size = 64
cache_max_gb = 4
chunk_tokens = 200
vector_precision = "f32"    # f32 | f16
doc_vectors = false         # also embed each item's doc comment; used only with the reranker

[languages]
tree_sitter_config = "~/.config/tree-sitter/config.json"
extensions = { "cljc" = "clojure" }
index_untagged_as_text = ["md", "txt", "adoc", "rst"]

[rerank]
enabled = false             # cross-encoder over the top candidates: ~0.4 s per search on Metal,
                            # ~2.6 s on CPU; lowers well-phrased queries' scores (see CHANGELOG)
hf_repo = "Alibaba-NLP/gte-reranker-modernbert-base"
candidates = 20             # use 50 with coderank or another weaker embedding model
max_tokens = 192
```

Built-in models (`lode models`) include `jina-code-v2`, `qwen3-0.6b`, `harrier-0.6b`, `coderank`,
`granite-r2`, `granite-small-r2`, `ultrabert-150m`, `embeddinggemma-2`, `minilm-l6` and, under a
non-commercial licence, `jina-code-0.5b`. Rerankers are chosen by Hugging Face repo in
`[rerank] hf_repo`; the comparison in the CHANGELOG lists which were measured and how they did.
Changing the embedding model means a full re-index (`lode index --force`); the embedding cache
keeps each model's vectors apart.

Adding a language never needs code: clone a grammar into one of tree-sitter's
`parser-directories` and lodestone loads it with its `queries/tags.scm`. Supplementary queries
(`tags.scm`, `tests.scm`) can be dropped into `~/.lodestone/queries/<language>/`.

## Run the evaluation

Fetch the example corpus (see `eval/README.md`):

```bash
git clone https://github.com/stacks-network/stacks-core eval/corpus && git -C eval/corpus checkout 95c6a45eed
```

Retrieval quality, index time and query latency:

```bash
lode bench eval --hard q06,q07,q08,q18
```

The first summary line is the agent view: the share of questions whose answer is within the
top 24 results an agent receives by default. rank@1/5/10 and MRR follow and measure ranking
accuracy.

Compare models or backends with `--model` and `--backend`; `--rerank` (or `--reranker`,
`--rerank-candidates`) enables the cross-encoder. The question file schema is generic (`[{id, kind, question, answers:[{path,
symbol}]}]`), so other repositories can be benchmarked with `--corpus` and `--questions`.

Agent-level evaluation (headless Claude Code, 18 tasks × 3 reps) with the daemon running and the
corpus registered:

```bash
python3 eval/harness/run_trials.py <scratch-dir> --file tasks.json --arms rule_lode --reps 3
```

```bash
python3 eval/harness/score.py <scratch-dir> tasks.json
```

`eval/harness/README-lode.md` describes the scratch-directory layout the harness expects.
