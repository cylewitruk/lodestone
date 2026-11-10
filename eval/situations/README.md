# Agent situations experiment

## Goal

Find out whether coding agents pick up lodestone through its skill, use it at the right moments
and in the intended way, and whether that makes them more successful or cheaper than searching
with Grep/Glob/Read alone, across four situations.

## Situations (stacks-core `95c6a45eed`)

| Situation | Tasks | The agent is asked to | Correct when | Lodestone should be used |
| ---- | ---- | ---- | ---- | ---- |
| write | `../tasks.json` (18) | write a helper as a code block | the code calls the existing helper (`accept` regex) | before writing, with a query built from the intent |
| review | `review.json` | review a diff | it names the existing code a duplicated function should reuse, or says none for controls | for each new definition, with a query built from the new code |
| refactor | `refactor.json` | consolidate a named function | it names the existing equivalent(s) elsewhere | with a query built from the function itself |
| locate | `locate.json` | find the code behind a behaviour or a message | it names the responsible function | for behaviour; Grep should come first for literal strings |

## Arms

| Arm | Tools | Lodestone | Skill |
| ---- | ---- | ---- | ---- |
| `none` | Read, Grep, Glob, Skill | — | — |
| `tools` | same | `search`, `status` over MCP | — |
| `skill` | same | `search`, `status` over MCP | the lodestone skill only (`plugin-skill/`) |
| `hook` | same | `search`, `status` over MCP | the shipped plugin (`../../plugin`): skill plus a SessionStart reminder |

`hook` was added after the first situation showed that, with the skill alone, agents searched in
6% of runs and never loaded the skill; its reminder now ships in the plugin. The shipped hook
adds the reminder only when the daemon serves the repository (`run.py` passes the daemon's port
as `LODESTONE_PORT`), which in this setup gives the same context the `hook` runs had.

No system-prompt rule in any arm: whether an agent searches is what is measured. User settings
are excluded (`--setting-sources project`) so the user's own skills and CLAUDE.md do not leak in.

## Metrics per run

Correctness; tokens, cost, wall time, turns and tool calls by tool; whether the skill was
loaded; number of searches and whether the first came before the agent read candidate code;
query form (combined = a sentence plus at least one identifier-like word); for `locate` literal
tasks, whether Grep came before `search`.

## Success criteria

1. `skill` is at least as correct as `none` in every situation, and better in write, review and
   refactor.
2. `skill` costs no more tokens and time than `none` per correct answer.
3. In `skill`, agents search in at least 80% of write, review, refactor and behaviour-locate
   runs, with combined queries in at least 70% of them, and use Grep first on literal-string
   tasks.
4. The gap between `tools` and `skill` measures what the skill adds on top of the tool
   descriptions.

## Results (Sonnet, 3 reps, 624 runs, 2026-11-09)

| Situation | Arm | Correct | Runs that searched | Median tokens | Median secs | Cost per correct |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| write | none | 25/54 | — | 38.8k | 13 | $0.100 |
| write | tools | 25/54 | 4% | 41.0k | 14 | $0.109 |
| write | skill | 24/54 | 6% | 54.1k | 15 | $0.113 |
| write | **hook** | **37/54** | 50% | 49.9k | 14 | **$0.071** |
| review | none | 33/36 | — | 73.6k | 19 | $0.078 |
| review | tools | 34/36 | 25% | 60.0k | 15 | $0.061 |
| review | skill | 34/36 | 31% | 44.1k | 13 | $0.049 |
| review | hook | 33/36 | 69% | 43.5k | 11 | $0.045 |
| refactor | none | 30/30 | — | 59.9k | 13 | $0.059 |
| refactor | tools | 30/30 | 17% | 54.8k | 12 | $0.054 |
| refactor | skill | 30/30 | 43% | 58.0k | 12 | $0.052 |
| refactor | hook | 30/30 | 97% | 44.0k | 11 | $0.045 |
| locate | none | 36/36 | — | 38.8k | 9 | $0.038 |
| locate | tools | 36/36 | 6% | 41.0k | 9 | $0.042 |
| locate | skill | 36/36 | 8% | 40.3k | 8 | $0.038 |
| locate | hook | 36/36 | 3% | 40.9k | 8 | $0.036 |

- **Agents do not discover lodestone on their own.** With the tools, or the tools and the skill,
  they searched in 4–43% of runs, and the skill was never loaded in any run. The skill's
  description still shifts behaviour a little (refactor 17% → 43%), but its body is never read.
- **A SessionStart reminder fixes adoption where it matters**: searches in 50% of write, 69% of
  review and 97% of refactor runs, all with combined queries (the format the tool description
  asks for), and rarely in locate, where Grep is the right tool. On write tasks it lifts
  correctness from 25/54 to 37/54 (+0.22 per task [+0.09, +0.37]; 8 tasks gain, none lose),
  but the same reminder without lodestone does as well (see Controls below), so the gain is the
  advice, not lodestone. Runs that searched were correct 22/27 times against 15/27 for those that
  did not, which is observational, not causal.
- **Timing:** in write and review the search is usually the first tool call (19 of 27 and 18 of
  25 searching runs); in refactor agents read the target first and then search, as intended.
  On literal-string locate tasks every arm went to Grep first.
- **Cost:** per correct answer, the hook arm is the cheapest in every situation; in write it
  uses more tokens per run (49.9k against 38.8k) but turns them into more correct answers.
- **Ceiling and confounds:** review, refactor and locate are at or near 100% in every arm, so
  they show cost and adoption but not accuracy; Grep and Claude's knowledge of stacks-core are
  enough for them. In review, stacks-core's own `stacks-pr-review` skill loads in 31 of 36
  runs without lodestone but in 9 (skill) and 3 (hook) with it, which explains most of review's
  token savings, and means lodestone's prompts crowd out a repository's own review skill.

### Skill descriptions (write, no reminder)

Three descriptions for the same skill body (`skill-variants/`), against the original:

| Arm | Description | Skill loaded | Runs that searched | Correct | vs none (per task) |
| ---- | ---- | ---- | ---- | ---- | ---- |
| `skill` | "Find existing code by what it does with lodestone's `search` tool. Use before writing…" | 0/54 | 6% | 24/54 | −0.019 [−0.093, +0.056] |
| `skill-v2` | "Use before writing any new function, type or helper in this repository, …" | 0/54 | 9% | 30/54 | +0.093 [−0.056, +0.278] |
| `skill-v3` | v2, named `find-existing-code` | 0/54 | 9% | 27/54 | +0.037 |
| `skill-v4` | "Use when asked to write, add or implement a function, helper, type or feature in this repository, or to review, refactor or deduplicate code. …" | 0/54 | **48%** | **36/54** | **+0.204 [+0.056, +0.389]** |
| `hook` | original skill plus the SessionStart reminder | 0/54 | 50% | 37/54 | +0.222 [+0.093, +0.370] |

(Write grading ignores code comments since an outside review; that moved one `skill-v3` pass.)
No variant was ever loaded; a probe confirmed the agent sees each description verbatim. But the
description in the skill listing acts as an instruction on its own: phrased around the request
the agent has just received ("Use when asked to write…"), like `stacks-pr-review`'s "Use when
asked to review…", it makes agents call `search` directly about as often as the reminder does,
with the same gain in correctness. Phrased as a precondition ("Use before writing…") it barely
registers.

Without the reminder, v4 across all four situations (`hook` = original description plus the
reminder):

| Situation | Runs that searched: v4 / hook | Correct: v4 / hook / none | Repo's own review skill loaded: v4 / hook / none |
| ---- | ---- | ---- | ---- |
| write | 48% / 50% | 36 / 37 / 25 of 54 | — |
| review | 44% / 69% | 34 / 33 / 33 of 36 | 9 / 3 / 31 of 36 |
| refactor | 77% / 97% | 30 / 30 / 30 of 30 | — |
| locate | 8% / 3% (literal: 0% / 0%) | 36 / 36 / 36 of 36 | — |

v4 alone matches the reminder on write tasks, but leaves 20–25
points of adoption on the table in review and refactor, and still crowds out the repository's
review skill (9 runs of 36 instead of 31). The plugin therefore keeps both.

The shipped skill now uses v4's description. `plugin-skill/` links to the shipped skill, so the
`skill` and `hook` rows in the tables above, recorded with the original description, would differ
if rerun.

### Controls (write)

Added after an outside review: the reminder's advice without lodestone (`remind`, `plugin-remind/`,
Grep only) and the shipped plugin (`shipped`: v4 description plus reminder).

| Arm | Correct | Answer seen in a tool result | Seen but not used | Calls to find (median) | Tokens per run (median) | Cost per correct |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| none | 25/54 | 24 | 4 | 1 | 38.8k | $0.100 |
| `remind` (no lodestone) | **37/54** | 40 | 6 | 1 | 51.2k | $0.079 |
| `hook` | 37/54 | 40 | 6 | 1 | 49.9k | $0.071 |
| `skill-v4` | 36/54 | 36 | 3 | 1 | 49.6k | $0.072 |
| `shipped` | 34/54 | 34 | 3 | 1 | 55.2k | $0.081 |

`remind` vs none: +0.222 [+0.074, +0.389]; `shipped` vs `remind`: −0.056 [−0.204, +0.074]. The
write gain comes from the advice to look for existing code; lodestone adds nothing measurable on
these tasks, in correctness or effort (`effort.py`). Agents find stacks-core helpers with a single
Grep of guessed names, which the model largely knows; the four hard write cases (t06, t07, t08,
t18) stay unsolved in every arm (0–2 of 12). Showing lodestone's value needs tasks where guessing
names fails.

## Pilot: realistic tasks on hard targets (`pilot.json`)

Nine tasks written as real tickets, reviews and bug reports, with no reuse wording and no names of
the target or its types (2 write, 2 review + 1 control, 2 refactor, 2 locate), built and then
independently reviewed with grep only. One run per arm; the reminder fired in all 18 runs that had
one; review and refactor answers were checked by hand (the review control by hand only).

| Task | none | remind | shipped |
| ---- | ---- | ---- | ---- |
| pw1 write (bit-packing helper) | ✗ | ✗ | ✗ |
| pw2 write (exact-size buffer) | ✗ | ✓ | ✗ |
| pv1 review (re-implemented reward-set lookup) | ✗ | ✗ | ✗ |
| pv2 review (hand-rolled base58 address) | ✗ | ✓ | ✓ (searched) |
| pv3 review control (new feature) | ✓ | ✓ | ✓ |
| pr1 refactor (compact-size vs `VarInt`) | ✓ | ✓ | ✓ (searched) |
| pr2 refactor (`to_txid` vs `from_bitcoin_tx_hash`) | ✓ | ✓ | ✗ |
| pl1 locate | ✓ | ✓ | ✓ |
| pl2 locate | ✓ | ✓ | ✓ |
| **Correct** | 5/9 | 7/9 | 5/9 |

- **Off the ceiling for write and review**, still on it for locate (6/6 in every arm) and nearly
  so for refactor.
- **Lodestone was used in 2 of 9 shipped runs**, despite the reminder; agents grepped first.
- **pw1:** no arm used a single tool; every agent wrote the code from scratch.
- **pv1:** no reviewer looked for existing code at all; reviews were about correctness.
- **Effort** was similar across arms (review runs 110–165k tokens, locate about 55k).
- No sign yet that lodestone helps on these tasks; one run per cell can only hint.

### Pilot on other models and effort levels

The same 27 cells on Opus 5.5 at medium effort (Claude Code's default) and Sonnet at high effort,
with the Agent tool also allowed (`run.py --model … --effort … --subagents`). Review and refactor
answers checked by hand; no review-control run claimed false reuse, so pv3 passes everywhere.

| Setup | Correct: none / remind / shipped | Shipped runs that searched | Write (pw1, pw2) correct: none / remind / shipped | Cost, 27 runs |
| ---- | ---- | ---- | ---- | ---- |
| Sonnet, medium (above) | 5 / 7 / 5 | 2/9 | 0 / 1 / 0 of 2 | $2.29 |
| Sonnet, high | 6 / 8 / 6 | 5/9 | 0 / 2 / 1 of 2 | $3.34 |
| Opus 5.5, medium | 6 / 6 / **8** | **9/9** | 0 / 0 / **2** of 2 | $6.34 |

- **Opus used lodestone in every shipped run**, one search each, all combined queries, usually as
  the first call (both write tasks, both locate tasks) and in refactor right after a Grep. Of the
  8 tasks with an answer, it was in the results for 6 (not pl1 or pv1). It is the only setup that
  solved both write tasks, and only with lodestone: with the same reminder and Grep alone it
  solved neither.
- **Higher effort moves Sonnet partway**: 2/9 → 5/9 shipped runs searched, with the answer in the
  results for 3 of 4 positive tasks. With the reminder alone, Sonnet at high solved both write
  tasks too.
- **pv1 is unsolved everywhere**: only Opus with lodestone looked for an existing lookup, and its
  query did not surface it; every other reviewer checked correctness only.
- **No setup used a subagent** (0 of 54 runs). A probe showed an Explore subagent gets lodestone's
  tools and skill listing but not the SessionStart reminder, so delegated searches would miss it.
- **Harness leak:** `--setting-sources project` does not stop the user's `~/.claude/CLAUDE.md`
  from loading. Every run in the tables above (all arms) saw the author's global instructions,
  including "adhere to DRY and SOLID". The leak is the same in every arm, but it likely raises
  reuse in all of them. `run.py` now excludes that file (`claudeMdExcludes` via `--settings`);
  the run below is the first without it.

## Situational run: Opus 5.5, stacks-core and LibreChat (`sit-*.json`)

**Corrected (LibreChat `shipped` re-run).** In the first run both batches ran at once and wrote
their MCP config to the same file, so all 19 LibreChat lodestone searches went to the stacks-core
daemon (the reminder, which reads its port from the environment, still fired). `run.py` now
writes one MCP config per port, and the 28 LibreChat `shipped` runs were re-run; all 24 of their
searches hit the LibreChat index. The tables below the corrected ones are the original, invalid
pooled figures, kept for the record.

| Positives correct (corrected) | none | remind | shipped |
| ---- | ---- | ---- | ---- |
| stacks-core | 17/24 | 22/24 | 23/24 |
| LibreChat | 10/24 | 15/24 | **20/24** |
| write | 3/12 | 7/12 | 9/12 |
| review | 6/12 | 10/12 | 12/12 |
| refactor | 6/12 | 8/12 | 10/12 |
| locate | 12/12 | 12/12 | 12/12 |
| **all** | **27/48** | **37/48** | **43/48** |

Per task: **`shipped` vs `remind` +0.125 [+0.000, +0.271]** (5 gained, 1 lost); on LibreChat
alone +0.208 [+0.042, +0.417] (4 gained, 0 lost). `shipped` vs none +0.333 [+0.167, +0.521];
`remind` vs none +0.208 [+0.062, +0.375]. All 8 `shipped` control runs passed.

| Efficiency (corrected) | none | remind | shipped |
| ---- | ---- | ---- | ---- |
| Cost, 56 runs | $10.35 | $10.23 | $9.92 |
| Cost per correct answer | $0.296 | $0.227 | $0.194 |
| Wall time, 56 runs | 1,801 s | 1,768 s | 1,677 s |
| Answer seen in a tool result | 28/48 | 37/48 | 43/48 |
| Median tokens until the answer was seen | 28k | 25k | 13k |

Paired per task, `shipped` / `remind`: **wall time ×0.93 [×0.89, ×0.98]**, cost ×0.97 [×0.91,
×1.03], tokens ×1.01 [×0.92, ×1.11], tool calls ×1.06 [×0.98, ×1.15], tokens until the answer was
seen ×0.90 [×0.76, ×1.09].

- **With lodestone working on both repositories, it adds correctness on top of the advice**:
  +0.125 per task overall (the interval just touches zero) and +0.21 on LibreChat, where Claude
  knows the code less well, with no task lost. Runs are 7% faster and cost per correct answer is
  15% lower.

Original, invalid figures:

28 realistic tasks, 14 per repository (`sit-stacks.json` = the 9 pilot tasks plus 5;
`sit-librechat.json`, LibreChat `0e719592`): 3 write, 3 review + 1 control, 3 refactor +
1 control, 3 locate each, written with grep only and independently reviewed. Opus 5.5 at medium
effort, Agent tool allowed, user CLAUDE.md excluded, 3 arms × 2 runs = 168 runs, one daemon per
repository. `remind` and `shipped` now share the same advice text word for word; `shipped`
adds two sentences pointing at lodestone (`plugin/hooks/session-start.sh`).

```bash
python3 run.py <out> --repo ../corpus --port 4611 --situations sit-stacks \
  --arms none,remind,shipped --reps 2 --model opus --effort medium --subagents
```

Review and refactor positives were checked by hand (every mention of an answer proposed it as
the existing equivalent); all 24 control runs passed (no false "use existing X"; the few
suggestions pointed at real libraries such as chrono).

| Positives correct | none | remind | shipped |
| ---- | ---- | ---- | ---- |
| stacks-core | 17/24 | 22/24 | 23/24 |
| LibreChat | 10/24 | 15/24 | 18/24 |
| write | 3/12 | 7/12 | 8/12 |
| review | 6/12 | 10/12 | **12/12** |
| refactor | 6/12 | 8/12 | 9/12 |
| locate | 12/12 | 12/12 | 12/12 |
| **all** | **27/48** | **37/48** | **41/48** |

Per task (paired, 95% bootstrap over the 24 positive tasks): `remind` vs none +0.208
[+0.062, +0.375] (6 tasks gained, 0 lost); `shipped` vs none +0.292 [+0.125, +0.479] (8, 0);
**`shipped` vs `remind` +0.083 [−0.021, +0.229]** (3 gained: pw1, lv1, lr3; 1 lost: pw2).

| Efficiency | none | remind | shipped |
| ---- | ---- | ---- | ---- |
| Cost, 56 runs | $10.35 | $10.23 | $10.28 |
| Cost per correct answer | $0.296 | $0.227 | $0.210 |
| Wall time, 56 runs | 1,801 s | 1,768 s | 1,747 s |
| Answer seen in a tool result | 28/48 | 37/48 | 42/48 |
| Median tokens until the answer was seen | 28k | 25k | 27k |

Paired per task, `shipped` / `remind`: cost ×1.01 [×0.94, ×1.08], wall time ×0.97 [×0.92,
×1.03], tokens ×1.10 [×0.98, ×1.22], tool calls ×1.12 [×1.01, ×1.23], tokens until the answer
was seen ×1.22 [×0.95, ×1.58].

- **Lodestone adds a little correctness and no efficiency.** With identical advice, the plugin
  solves 4 more of 48 than the advice alone (+0.08, not significant), mostly where guessing names
  fails (lv1 was solved only with lodestone, pw1 only with lodestone here and in the pilot). It
  costs and takes the same per task, uses a few more tool calls, and is not faster at finding the
  answer. Cost per correct answer is 7% lower, from the extra correct answers alone.
- **Most of the gain is the advice.** No lodestone → advice: +0.21; advice → advice + lodestone:
  +0.08.
- **Adoption on Opus is high**: the plugin's agents searched in all 28 stacks-core runs and 23 of
  28 LibreChat runs, always with combined queries. No run used a subagent (0 of 168).
- **The repository's review skill is no longer crowded out**: `stacks-pr-review` loaded in 8 of 8
  stacks-core review runs in every arm (against 3–9 of 36 with the earlier reminder on Sonnet).
- **Still at the ceiling:** locate (12/12 everywhere, even with the harder pl3 and LibreChat
  reports). Unsolved by every arm: lw3, lr1.

## Private repository: advice vs plugin (Opus 5.5)

The same design on a private Rust workspace the model cannot have seen (an embedded UI toolkit,
about 185k lines in 15 crates, 16.9k indexed definitions): 12 tasks (3 write, 3 review +
1 control, 3 refactor + 1 control, 1 locate), written with grep only and independently
reviewed, × `remind` and `shipped` × 2 runs = 48 runs. The task file stays outside the
repository (`SITUATIONS_TASKS=<dir> run.py … --situations <name>`), since its diffs quote
private code.

| | `remind` (advice, Grep) | `shipped` (advice + lodestone) |
| ---- | ---- | ---- |
| Positives correct | 19/20 | 20/20 |
| Controls passed (by hand) | 4/4 | 4/4 |
| Runs that searched | — | 22/24 |
| Cost, 24 runs | $6.26 | $6.02 |
| Wall time, 24 runs | 859 s | 845 s |
| Answer seen in a tool result | 19/20 | 20/20 |
| Median tool calls until the answer was seen | 4 | 2 |
| Median tokens until the answer was seen | 41k | 18k |

Paired per task, `shipped` / `remind`: **tokens until the answer was seen ×0.67 [×0.50, ×0.87]**
(10 tasks found in both arms); whole run: cost ×0.95 [×0.85, ×1.06], wall time ×0.97
[×0.86, ×1.10], tokens ×0.98 [×0.80, ×1.19], tool calls ×1.04 [×0.85, ×1.31].

- **On unfamiliar code lodestone finds the answer sooner**: half the tool calls and about a third
  fewer tokens before the answer first appears, a significant difference; on the public
  repositories it was not faster (×1.22, n.s.).
- **That saving barely reaches the whole run** (cost −5%, n.s.): finding is a small part of a run;
  reading the code and writing the review or answer dominate.
- **Correctness is at the ceiling**: Opus with the advice and Grep solved 19 of 20. This codebase
  keeps shared helpers in predictable places (one interaction module, the same file as the
  change), so grepping its consistent vocabulary works even without knowing the names. A test of
  correctness on private code needs harder targets (scattered helpers, inconsistent naming).

## Replay of the agents' lodestone queries

All 78 queries agents sent in the plugin runs above (the 19 LibreChat ones re-run against the
LibreChat index), replayed at top 50 through the bench with the same jina-code-v2 index:

| Top 8 (no reranker) | As written | Sentence only | Guessed names only | As written, reranker (pool 20) | As written, top 16 |
| ---- | ---- | ---- | ---- | ---- | ---- |
| stacks-core (38) | 27 | 27 | 21/34 | 29 | 29 |
| LibreChat (19) | 18 | 16 | 16 | 16 | 18 |
| private Rust (21) | 14 | 12 | 11/19 | 15 | 15 |
| **all (78)** | **59** | 55 | 48/72 | 60 | 62 |

- **The query format is not the problem**: the agents' combined queries beat the sentence or the
  names alone. Neither the reranker (+1 of 78) nor a deeper list (+3 at top 16) recovers much,
  and restricting results to code (markdown sections fill about one top-8 slot per query on the
  private repository) changes nothing.
- **About a third of the 19 misses search for the wrong thing**: in reviews, agents describe the
  whole feature in the diff ("a builder that processes a transaction"), while the answer is the
  small piece the diff re-implements. The rest are vocabulary gaps the embedder does not bridge
  (a stipple fill the code names after its visual effect; a "flip and clamp" popup rule that
  the code calls placement).
- Agents' queries reach the answer less often than the hard set's rewrites (59/78 vs about 95%):
  realistic tasks hide the target inside larger work, which no retrieval setting fixes.

### Fix candidates for the misses (offline)

Code as the query (review: each new function in the diff, one search each; refactor: the moved
function's source, itself excluded from the ranking), against the agents' own wording on the same
18 review and refactor tasks:

| Top 8 | Tasks |
| ---- | ---- |
| Agents' wording (mean over their queries) | 13.5/17 |
| Code as the query | 13/18 |
| Either | **15/17** |

Code finds what wording missed when the agent searched for the surrounding feature (pv1, and a
private-repo review), and misses where the answer is a type or impl rather than a function (pr1)
or is only partly equivalent (pr2). It complements the wording rather than replacing it.

Several phrasings (two alternatives per query written by Haiku from the query alone, merged)
against longer single-query lists, all 78 queries:

| Answer shown | All (78) |
| ---- | ---- |
| Single query, top 8 (now) | 59 |
| Single query, top 16 / 20 / 24 / 32 | 62 / 65 / 67 / 69 |
| 3 phrasings, top 8 each (24 shown) | 64 |
| 3 phrasings merged (RRF), top 8 / 12 | 58 / 62 |

More phrasings do not bridge vocabulary gaps better than a longer list from one query. A result
line costs about 50 tokens, so 24 results instead of 8 add about 800 tokens per search.


## Two search tools and 24 results (Opus 5.5)

The replay suggested two fixes: show 24 results instead of 8, and search with a function's code
when reviewing or refactoring. Advice alone did not get code searched (0 of 7 searches in a
5-task sample, even after the skill was read), so `search` was split into `semantic_code_search
(query, example_identifiers)` and `similar_code_search(code)`, and `top_k` was removed from the
MCP schemas after agents asked for 8 to 12 results on every `similar_code_search` call. Old arm:
the previous plugin and binary (`search`, top 8), run alongside on a copy of the same index
(`run.py --plugin`).

One run per task (usage limits), write, review and refactor tasks on stacks-core, LibreChat and
the private repository, Opus 5.5 medium:

| Positives | Old | New | Per task, new − old |
| ---- | ---- | ---- | ---- |
| All (27) | 23/27 | 25/27 | +0.074 [+0.000, +0.185] |
| stacks-core | 8/9 | 9/9 | +0.111 |
| LibreChat | 7/9 | 7/9 | 0 |
| Private | 8/9 | 9/9 | +0.111 |

| New / old (paired over tasks) | Ratio |
| ---- | ---- |
| Tokens | ×0.90 [0.78, 1.06] |
| Wall time | ×0.96 [0.88, 1.04] |
| Tool calls | ×0.88 [0.78, 0.98] |
| Grep calls | ×0.84 [0.72, 0.99] |
| Cost | ×1.14 [1.04, 1.26] |

- `similar_code_search` was called in 9 of 18 review and refactor runs, never in write runs.
- The gains are pv1 (review) and sw1 (write); nothing regressed. lr1 and lw3 fail in both arms:
  their answers rank below 24 for the agents' queries.
- Controls (5 per arm, graded by hand): no reply claimed a false equivalent in either arm.
- The ×1.14 cost is mostly prompt-cache warmth, not the tools: the old arm's prompt was still
  cached from earlier runs, while the new prompt (changed tool schemas) started cold in 9 of 27
  runs (0 in the old arm). The first call's prompt is only about 430 tokens longer. Pricing every
  run's first call as a warm start gives ×1.04 [0.90, 1.21]. Billed cost per correct answer:
  $0.250 old, $0.263 new.

Offline follow-up on the cost (no agent runs):

| Results shown | Replay hits (78 agent queries) | Tokens per search |
| ---- | ---- | ---- |
| 8 | 59 | ~340 |
| 16 | 62 | ~670 |
| 24 | 67 | ~1,000 |

A result line averages 148 characters (about 42 tokens): location 31%, signature 45%, line range
8%, doc 12%. Capping the doc text at 60 characters saves 1%. Going from 24 to 16 results saves
about 330 tokens per search, about 0.5% of a median run, and loses 5 of 78 hits; 24 stays.
- An earlier 5-task sample found a self-match bug: an agent pasted the refactor target without
  its comments, so it came back as its own first result. Comments are now ignored.
