# Architecture

One daemon per user holds the embedding model and serves every registered worktree. Agents reach
it through `lode mcp` (stdio, one per session); the CLI and scripts use REST over a Unix socket.
Without a daemon, `--local` runs the same engine in-process.

```text
 agent harness ── lode mcp (stdio relay) ──┐
 lode CLI / scripts ── REST ───────────────┤  Unix socket (TCP when a port is set)
                                           ▼
 ┌──────────────────────────── lode serve ──────────────────────────────┐
 │ api/       MCP tools, /v1 REST, session leases                       │
 │ registry/  worktrees: canonical root → index                         │
 │ watch/     one watcher per worktree → indexing task                  │
 │ index/     manifest, items, vectors (usearch), lexical (tantivy)     │
 │ chunk/     tree-sitter tags → one item per definition; prose → text  │
 │ embed/     candle (Metal / CPU) | fastembed | remote                 │
 │ cache/     content-addressed embeddings, shared by all worktrees     │
 │ search/    filtered kNN + BM25 → fusion → collapse → rerank → top-k  │
 │ graph/     project units (Cargo) for `from_unit`                     │
 └──────────────────────────────────────────────────────────────────────┘
```

## Invariants

Each has a test in [`tests/it/invariants.rs`](../tests/it/invariants.rs).

1. **The engine never indexes on a query.** A search answers from the current snapshot, with a
   stale flag and the reason when it is behind. The daemon is what schedules indexing: a search
   that names a worktree activates it (watched, caught up, indexed if new), and an MCP search in
   a never-indexed worktree waits up to 30 s for its first index.
2. **One canonical key.** Paths are canonicalized once, in `registry/`. A worktree's id is a
   hash of its canonical root; manifests store paths relative to it.
3. **Single writer, snapshot readers.** One writer per worktree; readers hold an immutable
   snapshot that the writer replaces in one swap. A search never blocks on indexing and never
   sees a half-written index.
4. **Embeddings are a pure function of (model id, chunk text).** That pair, hashed, is the
   cache key, so unchanged text is never embedded twice, in any worktree.
5. **Filters never touch index state.** Scope filters (unit, language, paths, kinds, tests)
   apply inside the vector and lexical searches, so a narrow scope still fills its results, and
   leave the index byte-identical.

## Data

Everything lives under `~/.lodestone/` (`LODESTONE_HOME`):

| Path | Holds |
| ---- | ---- |
| `config.toml` | [configuration](configuration.md) |
| `registry.json`, `registry.lock` | registered worktrees; the lock serializes updates across processes |
| `cache.sqlite` | embeddings keyed by `blake3(model id ‖ chunk text)` |
| `models/` | downloaded model weights |
| `queries/<language>/` | user query overlays (`tags.scm`, `tests.scm`) |
| `worktrees/<id>/` | one index per worktree (below) |
| `worktrees/<id>.lock` | its writer lock: one process indexes it at a time; held while it is deleted, and kept after |
| `daemon.sock`, `daemon.log` | the daemon's socket and, when auto-started, its log |
| `hook-sessions/` | per-session markers written by the reminder hooks |
| `transcripts.sqlite` | what `lode transcript-stats` extracted |
| `update-check.json`, `update-check.lock` | the latest release, as of the last daily lookup; the lock lets one command start the next |

Per worktree:

| File | Holds |
| ---- | ---- |
| `manifest.json` | per-file hash, size, mtime and item ids; written last in each batch |
| `items.sqlite` | one row per item: location, kind, name, signature, doc; the source of output lines |
| `vectors.usearch` | HNSW index of item vectors (cosine) |
| `doc-vectors.usearch` | doc-comment vectors, only with `doc_vectors = true` |
| `lexical/` | tantivy BM25 index over name, signature, doc and body |
| `graph.json` | project units and their dependencies |
| `instance` | identifies this creation of the index, so a process notices another deleted or recreated it |

Because the manifest is written after items, vectors and lexical entries, a crash leaves each
file either fully indexed or re-indexed on the next scan.

## Indexing

1. Walk the worktree with git's ignore rules plus `.lodestoneignore` (see
   [daemon](daemon.md#ignored-files)).
2. For each file whose size, mtime or hash changed, chunk it: one item per definition from the
   grammar's `tags.scm` (with lodestone's overlays in `queries/`), each item's text being a
   header line, its doc comment and its source, cut at `chunk_tokens`. Files with no grammar or
   no definitions become prose chunks.
3. Look each chunk up in the cache; embed the misses in batches and store them.
4. Replace the file's items, vectors and lexical entries, then its manifest entry.
5. Publish the new snapshot.

The watcher (500 ms debounce) feeds the same loop; a git `HEAD` or ref change triggers a full
rescan, and a failed watcher falls back to a 60 s poll reported in `status`.

## Search

Embed the query, then over one snapshot:

1. kNN over item vectors (and doc vectors when enabled), filtered inside the index.
2. BM25 over the lexical index, filtered the same way (off by default: `lexical_weight = 0`).
3. Reciprocal rank fusion, then one row per item.
4. Optional cross-encoder rerank of the top candidates (off by default).
5. Top 24, formatted one line per item (`search/format.rs`).

The defaults, and why, are in the [decision log](decision-log.md).
