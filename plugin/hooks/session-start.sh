#!/bin/sh
# Adds a short lodestone reminder to the session context, but only when a lodestone daemon is
# serving the session's repository (LODESTONE_PORT overrides the default port).
port="${LODESTONE_PORT:-4600}"
dir="$(cd "${CLAUDE_PROJECT_DIR:-.}" 2>/dev/null && pwd -P)" || exit 0
repos="$(curl -sf --max-time 1 "http://127.0.0.1:$port/v1/repos")" || exit 0
served=no
for root in $(printf '%s' "$repos" | grep -o '"root":"[^"]*"' | sed 's/^"root":"//; s/"$//'); do
  case "$dir/" in "$root"/*) served=yes ;; esac
done
[ "$served" = yes ] || exit 0
cat <<'JSON'
{"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "Before writing any new function, type or macro, when reviewing a diff that adds functions, when consolidating duplicated code, and when locating the code behind a described behaviour, look for existing code that already does it, and reuse what fits instead of writing a new version. This repository is indexed by lodestone: its `semantic_code_search` tool finds code by what it does, which Grep cannot when you do not know the name, and its `similar_code_search` tool finds code that does the same job as a given function; call it once for each function a diff adds or a refactor moves (see the lodestone skill). Use Grep for exact strings."}}
JSON
