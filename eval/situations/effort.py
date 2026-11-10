#!/usr/bin/env python3
"""Search effort: how much work an agent spends before the answer first appears in a tool result.

Usage: effort.py <out-dir> [situation ...]   (default: write)

For each run of a task that has an answer, "found" is the first tool result containing the
answer (the write task's `accept` pattern, or an answer symbol). Reports per arm: how often the
answer was found, and how often it was found but not used; for found runs, the median tool calls,
search calls (Grep, Glob, lodestone `search`) and tokens up to and including the finding call;
and for all runs, the median search calls, tokens and seconds for the whole run.
"""
import json
import re
import statistics
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from run import tasks  # noqa: E402
from score import SEARCHES as LODE_SEARCHES, grade, parse  # noqa: E402

SEARCHES = {"Grep", "Glob"} | LODE_SEARCHES


def pattern(situation: str, t: dict):
    if "accept" in t:
        return re.compile(t["accept"])
    if not t.get("answers"):
        return None
    return re.compile("|".join(rf"\b{re.escape(a['symbol'])}\b" for a in t["answers"]))


def text_of(content) -> str:
    if isinstance(content, str):
        return content
    return "\n".join(c.get("text", "") for c in content if isinstance(c, dict))


def trace(path: Path, pat) -> dict | None:
    """Calls, searches and tokens up to the first tool result matching `pat`, or None."""
    calls, searches, tokens, seen = {}, {}, 0, set()
    n_calls = n_search = 0
    for line in path.read_text().splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        msg = ev.get("message", {})
        if ev.get("type") == "assistant":
            if msg.get("id") not in seen:  # one API call can stream several assistant events
                seen.add(msg.get("id"))
                u = msg.get("usage", {})
                tokens += sum(u.get(k, 0) for k in (
                    "input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"))
            for b in msg.get("content", []):
                if b.get("type") == "tool_use":
                    n_calls += 1
                    n_search += b["name"] in SEARCHES
                    calls[b["id"]], searches[b["id"]] = n_calls, n_search
        elif ev.get("type") == "user":
            for b in msg.get("content", []) if isinstance(msg.get("content"), list) else []:
                if b.get("type") == "tool_result" and pat.search(text_of(b.get("content", ""))):
                    i = b.get("tool_use_id")
                    return {"calls": calls.get(i, n_calls), "searches": searches.get(i, n_search), "tokens": tokens}
    return None


def med(xs):
    return statistics.median(xs) if xs else float("nan")


def main() -> None:
    out = Path(sys.argv[1])
    situations = sys.argv[2:] or ["write"]
    print("| situation / arm | runs | found | found, not used | correct | to find: calls | to find: searches "
          "| to find: tokens | run: searches | run: tokens | run: secs |")
    print("| ---- " * 11 + "|")
    for situation in situations:
        items = {t["id"]: t for t in tasks(situation)}
        for adir in sorted(p for p in (out / situation).iterdir() if p.is_dir()):
            rows = []
            for path in sorted(adir.glob("*.jsonl")):
                t = items[path.name.split(".")[0]]
                pat = pattern(situation, t)
                if pat is None:
                    continue
                r = parse(path)
                correct = bool(r["complete"] and grade(situation, t, r["final"]))
                rows.append((trace(path, pat), correct, r))
            if not rows:
                continue
            found = [f for f, _, _ in rows if f]
            unused = sum(1 for f, c, _ in rows if f and not c)
            print(f"| {situation} / {adir.name} | {len(rows)} | {len(found)} | {unused} "
                  f"| {sum(c for _, c, _ in rows)} | {med([f['calls'] for f in found]):.0f} "
                  f"| {med([f['searches'] for f in found]):.0f} | {med([f['tokens'] for f in found]) / 1000:.1f}k "
                  f"| {med([r['greps'] + r['globs'] + r['searches'] for _, _, r in rows]):.0f} "
                  f"| {med([r['tokens'] for _, _, r in rows]) / 1000:.1f}k "
                  f"| {med([r['secs'] for _, _, r in rows]):.0f} |")


if __name__ == "__main__":
    main()
