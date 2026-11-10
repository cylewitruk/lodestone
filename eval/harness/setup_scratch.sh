#!/usr/bin/env bash
# Prepares a scratch directory for run_trials.py: harness files, question/task files, an empty
# MCP config, an MCP config pointing at the lodestone daemon, and trial-repo -> the corpus.
# Usage: setup_scratch.sh <scratch-dir> [corpus-dir] [port]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
scratch="$1"
corpus="$(cd "${2:-$here/../corpus}" && pwd)"
port="${3:-4600}"
mkdir -p "$scratch/harness"
cp "$here/reuse-rule.md" "$here/lode-must.md" "$scratch/harness/"
cp "$here/../questions.json" "$here/../tasks.json" "$scratch/"
echo '{"mcpServers": {}}' > "$scratch/harness/mcp-empty.json"
printf '{"mcpServers": {"lodestone": {"type": "http", "url": "http://127.0.0.1:%s/mcp"}}}\n' "$port" > "$scratch/harness/mcp-lode.json"
ln -sfn "$corpus" "$scratch/trial-repo"
echo "scratch ready: $scratch (trial-repo -> $corpus)"
