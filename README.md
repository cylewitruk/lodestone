# lodestone

Local semantic code search for coding agents. One daemon per machine indexes any number of
repositories and worktrees, keeps them fresh from file events, and answers "what in this codebase
does X?" over MCP and HTTP in well under a second.

- **Definitions, not chunks:** tree-sitter tags queries for Rust, Python, TypeScript/JavaScript,
  Go and Markdown; any other grammar through tree-sitter's own config.
- **Embeddings:** jina-embeddings-v2-base-code on GPU (Metal) or CPU. A reranker and BM25 fusion
  are available but off by default.
- **Shared cache:** a content-addressed embedding cache, so another worktree of the same
  repository costs only chunking and hashing.
- **Agent-ready:** MCP tools, a skill and a session reminder for Claude Code and Codex, installed
  with one command.

## Quick start

Requires a stable Rust toolchain (edition 2024) and a C compiler.

```bash
cargo install --path .
lode serve                        # daemon on 127.0.0.1:4600
lode register ~/src/my-repo       # watch and index
lode search "retry an HTTP call with exponential backoff"
lode install                      # wire up Claude Code and/or Codex
```

Models download on first use into `~/.lodestone/models`.

## Writing queries

One sentence on what the code does, then a few guessed function or type names. Wrong guesses
still help.

```text
parse a duration such as "5m" or "2h" into seconds. parse_duration duration_from_str
```

## Agent integration

```bash
lode install                      # every harness whose CLI is on PATH, user scope
lode install --client codex       # one harness (repeat --client for several)
lode install --scope project      # files to commit in the current repository
lode uninstall                    # same flags; --dry-run prints the steps only
```

| Part | User scope | Project scope |
| ---- | ---- | ---- |
| Skill | `~/.agents/skills/lodestone/`, linked from `~/.claude/skills/` | `.agents/skills/lodestone/`, linked from `.claude/skills/` |
| MCP server | `claude mcp add` / `codex mcp add` | `.mcp.json` / `.codex/config.toml` |
| SessionStart reminder | `~/.claude/settings.json` / `~/.codex/hooks.json` | `.claude/settings.json` / `.codex/hooks.json` |

- **Skill location:** the shared `.agents/skills/`. Claude Code doesn't read a repository's
  `.agents/skills`, so it gets a symlink from `.claude/skills/`.
- **Reminder:** `lode hook session-start` prints it only when the daemon serves the session's
  repository (port 4600, or `LODESTONE_PORT`).
- **First use:** Codex asks you to trust the hook once (`/hooks`). At project scope, Codex must
  trust the project and Claude Code asks you to approve the `.mcp.json` server.
- **Plugin:** `plugin/` bundles the same skill, server and hook as a Claude Code plugin
  (`claude --plugin-dir plugin`); its hook needs `lode` on PATH.

**Measured effect** (Opus 5.5, details in [eval/situations/README.md](eval/situations/README.md)):

- 28 tasks on stacks-core and LibreChat, 48 runs per arm: correct 27/48 with no help, 37/48 with
  advice alone, 43/48 with the plugin, with 7% shorter runs.
- On a private repository the model has never seen: half the tool calls to find the answer.

### MCP tools

| Tool | Input | Use |
| ---- | ---- | ---- |
| `semantic_code_search` | `query` (one sentence), `example_identifiers` (guessed names) | find code by what it does |
| `similar_code_search` | `code` (one function or type; diff `+` prefixes are stripped) | find code that does the same job, e.g. in a review or refactor |
| `status` | `repo` | index freshness |

- **Optional inputs:** both searches take `repo`, `from_unit` (only code a given unit may depend
  on, per the build graph), `language` and `include_tests`.
- **Results:** 24 per search, with no `top_k` (agents asked for too few). REST and the CLI keep
  `top_k`.
- **Self-match:** `similar_code_search` leaves out the passed code's own definition.

### Result format

One line per item:

```text
stacks-common/src/util/retry.rs:25:12  lines 25-29  pub struct RetryReader<'a, R: Read>  — Wrap a Read so that we store a copy of what was read.
```

| Part | Meaning |
| ---- | ---- |
| `path:25:12` | the name's line and column, compiler style |
| `lines 25-29` | the definition's inclusive line range, doc comments above it excluded |
| rest | kind, name and signature, then the doc comment's first line |

All numbers are 1-based, and columns count characters. Language servers count from 0, so
subtract 1 from both. JSON results carry `line`, `line_end`, `name_line` and `name_column`.

## How it works

- **Daemon:** `lode serve` keeps the embedding model resident and reopens every registered
  worktree. It watches each worktree for file changes and catches up on changes made while it was
  down.
- **Registering:** `lode register` canonicalizes the path, adds it to
  `~/.lodestone/registry.json`, then starts watching and indexing it.
- **CLI:** every subcommand uses the daemon when it is running and runs in-process otherwise
  (`--local` forces in-process).
- **Stale indexes:** searches never index. A stale index answers from what it has, and
  `lode status` says why it is stale.
- **Ignored files:** files are walked with `.gitignore` and `.lodestoneignore` rules. Directories
  containing `CACHEDIR.TAG`, such as Cargo's `target/`, are skipped.
- **Linux:** large trees may need a higher `fs.inotify.max_user_watches`. If the watcher fails,
  lodestone falls back to a 60 s rescan and reports it in `status`.

## CLI

| Command | Purpose |
| ---- | ---- |
| `lode serve [--port N]` | run the daemon |
| `lode register <path>` | add, watch and index a worktree |
| `lode index <path> [--force]` | re-index a worktree (incremental unless `--force`) |
| `lode search <query>` | search a registered worktree |
| `lode status` | index freshness |
| `lode install` / `lode uninstall` | wire up Claude Code and Codex |
| `lode models` | configured models and available backends |
| `lode languages` | grammars and the extension map |
| `lode bench <eval-dir>` | retrieval quality, index time and latency |

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

`~/.lodestone/config.toml`. Every key is optional, and `LODESTONE_HOME` moves the data directory.

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
enabled = false             # ~0.4 s per search on Metal, ~2.6 s on CPU; hurts well-phrased queries
hf_repo = "Alibaba-NLP/gte-reranker-modernbert-base"
candidates = 20             # 50 for weaker embedding models such as coderank
max_tokens = 192

[search]
lexical_weight = 0.0        # BM25 fusion (reciprocal rank); off by default
identifier_lexical_weight = 0.0
```

**Backends:**

- **macOS:** the default build runs models on the GPU through candle and Metal.
- **Elsewhere:** candle on the CPU.
- **fastembed (ONNX Runtime):** an optional backend for the models it ships.

**Models:**

- **Built in (`lode models`):** `jina-code-v2` (default), `qwen3-0.6b`, `harrier-0.6b`,
  `coderank`, `granite-r2`, `granite-small-r2`, `ultrabert-150m`, `embeddinggemma-2`,
  `minilm-l6`, and `jina-code-0.5b` (non-commercial licence).
- **Rerankers:** any Hugging Face repo in `[rerank] hf_repo`. The CHANGELOG compares the ones
  measured.
- **Switching models** needs a full re-index (`lode index --force`). The cache keeps each model's
  vectors apart.

**Languages:**

- **Built-in grammars** are Cargo features, all on by default: `lang-rust`, `lang-python`,
  `lang-typescript`, `lang-go`, `lang-markdown`.
- **Adding one needs no code:** clone a grammar into one of tree-sitter's `parser-directories` and
  its `queries/tags.scm` is used.
- **Extra queries** (`tags.scm`, `tests.scm`) go in `~/.lodestone/queries/<language>/`.

## Evaluation

Fetch the example corpus:

```bash
git clone https://github.com/stacks-network/stacks-core eval/corpus && git -C eval/corpus checkout 95c6a45eed
```

Measure retrieval:

```bash
lode bench eval --hard q06,q07,q08,q18
```

- **Agent view (first line):** the share of questions whose answer is in the top 24 results.
- **Ranking accuracy:** rank@1/5/10 and MRR.
- **Comparing setups:** use `--model` and `--backend`; `--rerank`, `--reranker` or
  `--rerank-candidates` turn on the reranker.
- **Other repositories:** pass `--corpus` and `--questions`, with questions shaped
  `[{id, kind, question, answers: [{path, symbol}]}]`.

Agent evaluations (headless Claude Code) live in [eval/harness/](eval/harness/README-lode.md)
and [eval/situations/](eval/situations/README.md). Question sets and corpora are described in
[eval/README.md](eval/README.md). Measured results and decisions are in
[CHANGELOG.md](CHANGELOG.md).
