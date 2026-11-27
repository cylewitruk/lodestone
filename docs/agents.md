# Agents

## Setup

```bash
lode install                      # every harness whose CLI is on PATH, user scope
lode install --client codex       # one harness: claude, codex, antigravity (agy); repeatable
lode install --scope project      # files to commit in the current repository
lode uninstall                    # same flags; --dry-run prints the steps only
```

What it installs:

| Part | User scope | Project scope |
| ---- | ---- | ---- |
| Skill | `~/.agents/skills/lodestone/`, linked from `~/.claude/skills/` and `~/.gemini/config/skills/` | `.agents/skills/lodestone/`, linked from `.claude/skills/` |
| MCP server (`lode mcp`) | `claude mcp add` / `~/.codex/config.toml` | `.mcp.json` / `.codex/config.toml` |
| Reminder hooks | `~/.claude/settings.json` / `~/.codex/hooks.json` | `.claude/settings.json` / `.codex/hooks.json` |
| Antigravity plugin (MCP server and hook) | `~/.gemini/config/plugins/lodestone/` | `.agents/plugins/lodestone/` |

Antigravity takes a repository's MCP servers only from a plugin, so its server and `SessionStart`
hook come in one at both scopes. It names the server `lodestone_lodestone`: allow its tools with
`mcp(lodestone_lodestone/*)` (headless `agy -p` denies MCP calls without a rule).

**First use:** Codex asks you to trust the hook once (`/hooks`). At project scope, Codex must
trust the project and Claude Code asks you to approve the `.mcp.json` server.

**Plugin:** `plugin/` bundles the same skill, server and hooks as a Claude Code plugin
(`claude --plugin-dir plugin`). Its hooks need `lode` on PATH.

## MCP tools

| Tool | Input | Use |
| ---- | ---- | ---- |
| `semantic_code_search` | `query` (one sentence), `example_identifiers` (guessed names) | find code by what it does |
| `similar_code_search` | `code`: a list of functions or types (diff `+` prefixes are stripped) | find code that does the same job, e.g. for every function a diff adds |
| `status` | `repo` | index freshness |

Example call:

```json
{
  "query": "parse a duration such as \"5m\" or \"2h\" into seconds",
  "example_identifiers": ["parse_duration", "duration_from_str", "to_secs"]
}
```

### Options

Both searches also take:

| Input | Effect |
| ---- | ---- |
| `repo` | the repository to search; defaults to the session's |
| `from_unit` | only code a given package, crate or module may depend on, per the build graph |
| `language` | one language |
| `paths` | repo-relative globs, e.g. `["crates/foo", "!**/generated/**"]` |
| `include_tests` | include tests, ranked below other code |

- **Globs:** `*` stays within one directory and `**` crosses them. A directory matches everything
  under it, and a leading `!` excludes. The CLI takes `--path`, repeatable.
- **Filters** apply inside the search, so a narrow scope still returns a full set of results.
- **Result count:** 24 per search. REST and the CLI take `top_k` (default 24).
- **Several definitions:** `similar_code_search` returns a section per definition (24 results for
  one, fewer each for several, at least 5). Definitions matching the passed one (its own once
  indexed, or an existing copy) are listed apart, after the ranked results.

### Result format

```text
stacks-common/src/util/retry.rs:25:12  lines 25-29  pub struct RetryReader<'a, R: Read>  — Wrap a Read so that we store a copy of what was read.
```

| Part | Meaning |
| ---- | ---- |
| `path:25:12` | the name's line and column, compiler style |
| `lines 25-29` | the definition's line range, excluding doc comments above it |
| rest | kind, name and signature, then the doc comment's first line |

Numbers are 1-based and columns count characters; language servers count from 0. JSON results
carry `line`, `line_end`, `name_line` and `name_column`.

## How the integration behaves

- **Which repository:** `lode mcp` searches the session's repository when a call names none. In a
  folder holding several indexed repositories, calls must pass `repo`; the error lists them.
- **Worktree sessions:** Claude Code in the desktop app starts the MCP server in the main
  checkout. For a Claude session (`CLAUDE_CODE_SESSION_ID`), lodestone uses the directory the
  session's hooks recorded in `~/.lodestone/hook-sessions/`.
- **Client roots:** Antigravity starts the server in its plugin directory. A client on MCP
  `2026-07-28` or later that has roots is asked for them with each call, and the call searches the
  first, ahead of any hook marker. One server may serve several workspaces. If the client names
  no roots, calls must pass `repo`.
- **New worktrees:** the first search in a never-indexed worktree waits up to 30 s for its index.
- **Approval:** the tools are read-only (`readOnlyHint`). For Codex, `lode install` sets
  `default_tools_approval_mode = "approve"`. When the daemon serving them has said its embeddings
  are local, tool descriptions say nothing leaves the machine; until then they say nothing about
  it, and the harness is told when the list changes. Calls made under that promise are refused
  by a daemon whose embeddings are remote (one started since with another config).
- **Reminders:** `lode hook session-start` reminds the agent in a registered repository (or names
  the ones in the folder) and starts the daemon early. Hooks keep a marker per session in
  `~/.lodestone/hook-sessions/`.
  - Claude Code's desktop app skips SessionStart hooks, so `lode hook prompt-submit` adds the
    reminder to the first prompt instead.
  - Subagents don't see their session's reminder, so `lode hook subagent-start` adds it to each.
  - Antigravity gets the session-start reminder only: it has no subagent hook, and its
    `PreToolUse` hook cannot add context without deciding whether the tool runs.
  - Codex gets neither the nudge nor the edit hint: its edits arrive as multi-file `apply_patch`
    patches, which the hook would need its own parser for.
- **Nudge:** for Claude, `lode hook pre-tool-use` runs before each shell command. When it searches
  a directory tree lodestone serves for three or more names (`grep -rn 'a\|b\|c' .`), it adds a
  line pointing at `semantic_code_search`, with `repo` when the search is in another checkout.
  It says nothing for any other command, such as a `git grep` over another revision, and never
  blocks one: the installed command ends in `|| true`.
- **Edit hint:** the same hook runs before Claude's `Edit`, `MultiEdit` and `Write`. It chunks the
  new text with the file's grammar and searches the file's repository for each definition the
  edit adds or changes; tests and definitions holding others (an `impl` block, whose methods are
  each checked) are skipped. When an existing definition in the same language scores at least
  `[hooks] duplicate_similarity` (cosine, default 0.75), it adds one line naming up to three, as
  `path:line name`, each with the new definition it resembles. Definitions the edit replaces don't
  count. It needs a running daemon and says nothing on any error.
- **Your files:** `lode install` and `uninstall` change only the hook commands lodestone wrote (a
  hook of yours that runs `lode` stays), a `SKILL.md` lodestone shipped, and its own link to the
  shared skill. They check every step first, and change nothing if anything else is in the way:
  your own skill, or a file, directory or other link where the link goes, or an Antigravity
  `plugin.json` naming another plugin. The shared skill stays while another harness still has
  lodestone at that scope. Settings you added to
  the Codex server table stay, except HTTP-only ones when installing replaces the old HTTP server
  with the stdio one.
- **Deferred tools:** the search tools ask Claude Code to load them upfront
  (`_meta["anthropic/alwaysLoad"]`, v2.1.285 or later). Where they're still deferred, the reminder
  and skill say to load them with ToolSearch (`+lodestone search`).

## Usage statistics

How agents actually use lodestone, from their own transcripts:

```bash
lode transcript-stats --claude --codex --antigravity   # every harness's default directories
lode transcript-stats --codex ~/elsewhere --repo stacks-core --since 2026-11-01 --queries
```

Name at least one source; a flag without a directory uses the harness's default:
`~/.claude/projects`, `~/.codex/sessions`, and Antigravity's app data directories
`~/.gemini/antigravity` and `~/.gemini/antigravity-cli`. `--since` counts calls from a date,
`--queries` lists every search, `--json` is for machines.

**Reported** per repository and harness:

- sessions that searched, and whether the first search came before the first grep
- searches whose results were later read, that returned nothing, or that failed or were denied
- latency, and grep / glob / read calls
- for Claude Code, the share of sessions with the tools loaded rather than deferred
- for Claude Code, grep nudges and edit hints, and how many a lodestone search followed within
  5 minutes; edit hints count towards the session's repository

**What counts:**

- **Repository:** each call counts towards the registered repository it worked in (its file path
  or command directory, else its `repo`, else the session's directory). Linked worktrees count
  towards their repository, also when named as `repo`; calls elsewhere are dropped.
- **Shell commands:** a Claude shell command counts as the greps (`rg`, `grep`, `git grep`) and
  file reads (`cat`, `sed -n`, `head`, `tail`) it makes, each where it ran (after `cd`) or on the
  path it names. A grep that filters output (`cargo test | grep FAILED`) is not one. Antigravity's
  shell commands count the same way; Codex's count by Codex's own classification.
- **Antigravity sessions:** a conversation's directory is the first workspace its app's
  `conversation_summaries.db` records. Without one, only calls on a registered repository's files
  count.

**Storage:** `~/.lodestone/transcripts.sqlite` keeps call kinds and times, queries, result paths
and the paths of file reads; no other content. It reads only new lines of each transcript and
keeps what it extracted after the harness deletes old transcripts. Registering another repository,
or a new `lode` version, re-reads the transcripts still on disk; each keeps its data until it is
read again.
