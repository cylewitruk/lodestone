---
name: find-existing-code
description: Use before writing any new function, type or helper in this repository, when reviewing or refactoring code for duplication, and when looking for where a behaviour is implemented. Checks whether the code already exists, which Grep cannot do without knowing its name, and shows how to phrase lodestone `search` queries so they find it.
---

# lodestone

The `search` tool (MCP server `lodestone`) is a semantic index of this repository's definitions.
It finds code by what it does, which Grep cannot when you do not know the name. A search takes
about 10 ms and returns definitions with their location.

## When to search

| Situation | Search | Build the query from |
| ---- | ---- | ---- |
| Writing new code | before writing any helper, type, trait or macro | what you need it to do |
| Reviewing a diff | once for each new function or type the diff adds | what that new code does |
| Refactoring | for each function you consolidate, move or rewrite | what that function does |
| Locating behaviour | when a report describes behaviour | the behaviour |

Use Grep instead for exact strings: a quoted log or error message, a known identifier, a config
key. Grep the literal first and search only if it finds nothing.

## Writing the query

One sentence describing the behaviour in plain words, then three to five guessed function or type
names in the codebase's naming style. Wrong guesses still help.

```text
parse a duration such as "5m" or "2h" into seconds. parse_duration duration_from_str to_secs
```

When starting from existing code (review, refactor), describe what it does in one sentence, then
its own name and two or three names it might have elsewhere. Do not paste the code.

If the answer is not in the results, rephrase around the capability rather than your task's own
nouns ("rewind a reader to replay bytes already read", not "decode one of two message types")
and search again. Two or three searches is normal.

## Using the results

Each result is `path:line:col  lines start-end  kind name(signature) — doc`. Read the most
likely candidates before deciding. Reuse what fits instead of writing a new version; in a review,
name the existing function the diff should call. If results look stale or empty, call `status`.
