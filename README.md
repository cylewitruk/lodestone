<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/branding/readme-header-dark.svg">
    <img alt="lodestone: local semantic code search for coding agents" src="assets/branding/readme-header-light.svg" width="100%">
  </picture>
</p>

Ask "where in this codebase do we do X?" and get the function, not a grep hit. lodestone indexes
your repositories locally and gives Claude Code, Codex and you semantic code search in well under
a second.

## Features

- **Finds definitions:** results are functions, types and methods, parsed with tree-sitter.
- **16 languages built in:** Rust, Python, TypeScript/JavaScript, Svelte, Go, Java, C, C++, C#,
  Ruby, PHP, Lua, Elixir, Swift, Clarity and Markdown. Add more without code.
- **Runs locally:** embeddings are computed on your machine (Metal GPU on macOS), so your code
  stays there.
- **Stays fresh:** the daemon re-indexes on file changes in the worktrees agent sessions work in
  (every registered one with `lode serve`), and catches up when a session starts.
- **Cheap worktrees:** a shared embedding cache means a new worktree re-embeds only what changed.
- **Agent-ready:** one command sets up Claude Code, Codex and Antigravity: MCP tools, a skill and reminders.

## Install

macOS and Linux (x86_64, arm64):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/cylewitruk/lodestone/releases/latest/download/lodestone-cli-installer.sh | sh
```

Windows (x86_64):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/cylewitruk/lodestone/releases/latest/download/lodestone-cli-installer.ps1 | iex"
```

With Cargo ([crates.io](https://crates.io/crates/lodestone-cli); needs a C compiler and takes a few
minutes):

```bash
cargo install --locked lodestone-cli
```

Archives and checksums are on the [releases page](https://github.com/cylewitruk/lodestone/releases).

**Upgrading:** `lode upgrade` installs the latest release and stops the daemon so the next
command starts the new one. Installed with Cargo, run `cargo install` again, then `lode stop`.
Commands at a terminal say when a new release is out; set `LODESTONE_NO_UPDATE_CHECK=1` to turn
that off.

## Quick start

```bash
cd ~/src/my-repo
lode register                      # register this repository; indexing starts in the background
lode install                       # set up Claude Code and/or Codex
lode status                        # indexing progress; results are complete once it is done
lode search "retry an HTTP call with exponential backoff"   # searches the repository you are in
lode doctor                        # check the setup; prints the fix for anything wrong
```

There is nothing to keep running: the daemon starts when needed and exits when idle. The model
(about 300 MB) downloads on first use, so the first index takes longer. Every command is listed in [docs/daemon.md](docs/daemon.md#cli);
`lode <command> --help` has the details.

## Writing queries

Describe what the code does in one sentence, then guess a few names in the codebase's style.
Wrong guesses still help.

```bash
lode search 'keep a copy of the bytes read from a stream so they can be read again. RecordingReader TeeReader replay_reader'
```

Results, one per line (here from [stacks-core](https://github.com/stacks-network/stacks-core)):

```text
stacks-common/src/util/retry.rs:25:12  lines 25-29  pub struct RetryReader<'a, R: Read>  — Wrap a Read so that we store a copy of what was read.
libsigner/src/tests/http.rs:128:8  lines 128-136  fn MockHTTPSocket::read(&mut self, buf: &mut [u8]) -> io::Result<usize>
stacks-common/src/util/retry.rs:52:8  lines 52-57  fn RetryReader::read_and_buffer(&mut self, buf: &mut [u8]) -> io::Result<usize>
```

## With coding agents

```bash
lode install                       # every agent CLI on PATH, for your user
lode install --scope project       # files to commit in this repository instead
```

Agents get three MCP tools:

| Tool | Finds |
| ---- | ---- |
| `semantic_code_search` | code by what it does |
| `similar_code_search` | code that does the same job as given code, e.g. before writing a helper |
| `status` | how fresh the index is |

On 48 positive runs, Opus 5.5 solved 27 unaided, 37 with reuse advice, and 43 with the lodestone
plugin ([results](eval/situations/README.md#situational-run-opus-55-stacks-core-and-librechat-sit-json)).
See [docs/agents.md](docs/agents.md) for what `lode install` changes, the tool inputs and
`lode transcript-stats`.

## Documentation

| Page | Covers |
| ---- | ---- |
| [Agents](docs/agents.md) | `lode install`, MCP tools, result format, usage statistics |
| [Configuration](docs/configuration.md) | `config.toml`, models, backends, languages |
| [Daemon](docs/daemon.md) | every CLI command, lifecycle, worktrees, ignored files, REST API |
| [Architecture](docs/architecture.md) | components, invariants, on-disk data, indexing and search |
| [Evaluation](docs/evaluation.md) | measuring retrieval and agent quality |
| [Contributing](CONTRIBUTING.md) | building, testing, code style |
| [Releasing](docs/releasing.md) | how versions are published |
| [Changelog](CHANGELOG.md) | changes per release |
| [Decision log](docs/decision-log.md) | measurements and design decisions |

## License

MIT or Apache-2.0, at your option. The vendored Clarity grammar is MIT
([vendor/tree-sitter-clarity/LICENSE](vendor/tree-sitter-clarity/LICENSE)).
