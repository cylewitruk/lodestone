# Daemon, CLI and REST

One daemon per user keeps the model loaded and serves every registered repository.

## Modes

| | On demand (default) | Always on |
| ---- | ---- | ---- |
| Started by | `lode search`, `register`, `index`, `lode mcp` or the session hook | you: `lode serve` |
| Watches | repositories an agent session is in | every registered repository |
| Stops | `idle_timeout_secs` (30 min) after the last session, once indexing is done; or `lode stop` | `lode stop`, Ctrl-C or SIGTERM |

- **Sessions:** each `lode mcp` holds a lease on its repository, renewed every 30 s. CLI and REST
  searches take a shorter one.
- **Catching up:** a repository is re-scanned when a session starts using it.
- **Socket:** `~/.lodestone/daemon.sock` (mode `0600`), so only you can reach it. A lock file keeps
  it to one daemon.
- **TCP:** off unless `port` is set (or `LODESTONE_PORT`, `lode serve --port N`). Windows always
  uses TCP, on 4600 by default.
- **Log:** `~/.lodestone/daemon.log` when auto-started.

## Repositories and worktrees

- **Registering:** `lode register` adds the path to `~/.lodestone/registry.json` and starts its
  first index in the background; `lode status` shows when it is done.
- **Unregistering:** `lode unregister` (`lode rm`) removes a worktree, by name or path (a deleted
  one by its root), and deletes its index. It stops the daemon's indexing pass first; while another
  process indexes it (on Windows, has it open at all), or if the index cannot be deleted, it fails
  and stays registered. A removal interrupted part-way is finished by the next `lode rm`, by
  registering it again, or by the daemon. A linked worktree of a registered repository registers
  itself again on next use.
- **One index per worktree:** results never mix worktrees. They share an embedding cache, so a
  second worktree only re-embeds what differs.
- **Linked worktrees** (`git worktree add`) of a registered repository register themselves on
  first use as `<repository>:<worktree>`, and are unregistered once their directory is deleted.
- **Which one:** a session or command uses the worktree it runs in; `repo` overrides it. An
  unregistered checkout gets an error asking for `lode register`, never another checkout's
  results. A repository or linked worktree nested in a registered one is left out of its index;
  submodules are part of it.
- **Searching keeps a worktree fresh:** a search that names a worktree has the daemon watch it,
  catch it up and index it if new. The search itself answers from the index as it is: a stale
  index answers from what it has and `lode status` says why. An MCP search in a never-indexed
  worktree waits up to 30 s for its first index.
- **Chunking changes:** a full scan re-chunks only files whose chunking changed (a language's
  queries, `[languages]`, or `chunk_tokens`). Unchanged text reuses cached embeddings.

## Ignored files

`.gitignore` at every level, `.git/info/exclude`, the global git excludes file, `.ignore` and
`.lodestoneignore`. Hidden files, symlinks and directories containing `CACHEDIR.TAG` (such as
Cargo's `target/`) are skipped. Editing an ignore file, or creating or removing a checkout inside
the tree, triggers a full rescan. A directory or file that cannot be read keeps what was indexed
from it until a scan can read it again, and `lode status` names it until then. Rebuilding the
index (`lode index --force`, or after a model change) re-indexes every file a batch at a time,
so the index stays searchable and an unreadable file keeps its items.

On Linux, large trees may need a higher `fs.inotify.max_user_watches`. If the watcher fails,
lodestone rescans every 60 s and says so in `lode status`.

## CLI

| Command | Purpose |
| ---- | ---- |
| `lode serve [--port N]` | run the daemon in the foreground, always on; `--port` adds TCP |
| `lode stop` | stop the daemon gracefully |
| `lode mcp` | MCP server over stdio; starts the daemon on demand |
| `lode register [path]` | add, watch and index a worktree (default: the current checkout) |
| `lode unregister [name\|path]` | unregister a worktree and delete its index (alias `rm`; default: the current one) |
| `lode index [path] [--force]` | re-index a registered worktree (incremental unless `--force`) |
| `lode search <query>` | search a registered worktree |
| `lode status [--repo name\|path] [--json]` | index freshness of a worktree; elsewhere, a summary of all |
| `lode install` / `lode uninstall` | set up Claude Code, Codex and Antigravity ([agents](agents.md)) |
| `lode transcript-stats --claude --codex --antigravity` | how agents use lodestone ([agents](agents.md#usage-statistics)) |
| `lode models` | configured models and available backends |
| `lode languages` | grammars and the extension map |
| `lode bench <eval-dir>` | retrieval quality, index time and latency ([evaluation](evaluation.md)) |
| `lode upgrade [--force]` | install the latest release and stop the daemon (alias `update`; `--force` reinstalls it); installers' installs only |

`search`, `register` and `index` start the daemon if needed. `status` and `unregister` never start
one; without a daemon they act on the indexes directly. `--local` runs everything in-process.

Once a day, a command run at a terminal looks up the latest GitHub release in the background and
later ones print a line on stderr while it is newer. `mcp`, `serve`, hooks and `--json` output
never do. `LODESTONE_NO_UPDATE_CHECK=1` turns it off. `LODESTONE_GITHUB_TOKEN` authenticates the
lookups (and `lode upgrade`'s) where GitHub's per-IP limit for anonymous calls is shared.

## REST

Over the socket (`curl --unix-socket ~/.lodestone/daemon.sock http://localhost/v1/repos`), or on
`127.0.0.1:<port>` when TCP is on. MCP over HTTP is at `/mcp`.

```text
GET  /v1/search?q=&repo=&from_unit=&language=&path=&top_k=&include_tests=&with_body=&format=text|json
POST /v1/search   {query, repo, from_unit, language, paths, top_k, include_tests, kinds, with_body}
GET  /v1/status?repo=&format=text|json
GET  /v1/status/all?format=text|json
POST /v1/register {path}
POST /v1/unregister {repo, cwd}
POST /v1/index    {repo, force}
GET  /v1/repos
```

Internal, used by `lode mcp`, the hooks and `lode stop`:

```text
POST   /v1/sessions        {id, cwd, ttl_secs}               lease the worktree containing cwd
DELETE /v1/sessions/{id}                                     release it
POST   /v1/shutdown                                          stop gracefully
POST   /v1/tool            {name, arguments, session, cwd}   run an MCP tool
```
