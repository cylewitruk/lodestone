---
name: lodestone
description: Use when asked to write, add or implement a function, helper, type or feature in this repository, or to review, refactor or deduplicate code. Finds existing code that already does it (Grep only finds names you already know) and shows which lodestone search to use and how to phrase it.
---

# lodestone

lodestone (MCP server `lodestone`) is a semantic index of this repository's definitions with two
search tools. Both find code by what it does, which Grep cannot when you do not know the name. A
search takes about 10 ms and returns definitions with their location.

## When to search

| Situation | Tool | Input |
| ---- | ---- | ---- |
| Writing new code | `semantic_code_search`, before writing any helper, type, trait or macro | what you need it to do |
| Reviewing a diff | `similar_code_search`, once for each new function or type the diff adds | that function's code |
| Refactoring | `similar_code_search`, for each function you consolidate, move or rewrite | that function's code |
| Locating behaviour | `semantic_code_search`, when a report describes behaviour | the behaviour |

Use Grep instead for exact strings: a quoted log or error message, a known identifier, a config
key. Grep the literal first and search only if it finds nothing.

## semantic_code_search

`query` is one sentence describing the behaviour in plain words; `example_identifiers` are three
to five guessed function or type names in the codebase's naming style. Wrong guesses still help.

```json
{"query": "parse a duration such as \"5m\" or \"2h\" into seconds",
 "example_identifiers": ["parse_duration", "duration_from_str", "to_secs"]}
```

If the answer is not in the results, rephrase around the capability rather than your task's own
nouns ("rewind a reader to replay bytes already read", not "decode one of two message types")
and search again. Two or three searches is normal.

## similar_code_search

`code` is the source of one function or type, as written (diff `+` prefixes are fine). Pass one
definition per call, not a whole diff. The passed code's own definition is left out of the
results. If it finds nothing, describe the function with `semantic_code_search`.

## Using the results

Each result is `path:line:col  lines start-end  kind name(signature) — doc`. Read the most
likely candidates before deciding. Reuse what fits instead of writing a new version; in a review,
name the existing function the diff should call. If results look stale or empty, call `status`.
