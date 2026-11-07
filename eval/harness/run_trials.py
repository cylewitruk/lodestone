#!/usr/bin/env python3
"""Run the helper-exists pilot: each (arm, question) is one headless `claude -p` run.

Usage: run_trials.py <scratchpad> [--arms stock,rule,rule_ck,rule_lode] [--model sonnet] [--jobs 4] [--only q01,q02]
"""
import argparse
import json
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

TASK_SUFFIX = (
    "\n\nReply with the complete Rust code in one fenced block (include the `use` lines it needs). "
    "Do not edit any files."
)

SUFFIX = (
    "\n\nSearch this repository to find out, then answer briefly. End your reply with one final line in "
    "exactly this form: `ANSWER: <repo-relative path>::<symbol>` naming the best existing helper, or "
    "`ANSWER: none` if nothing suitable exists."
)


def run_one(scratch: Path, arm: str, item: dict, model: str, rep: int, runs: str) -> None:
    out = scratch / runs / arm / f"{item['id']}.r{rep}.jsonl"
    if out.exists() and out.stat().st_size > 0:
        return
    out.parent.mkdir(parents=True, exist_ok=True)
    cmd = [
        "claude", "-p", item["question"] + (TASK_SUFFIX if item["kind"] == "task" else SUFFIX),
        "--model", model,
        "--output-format", "stream-json", "--verbose",
        "--tools", ("Read,Grep,Glob,Skill" if arm == "rule_skill" else "Read,Grep,Glob"),
        "--strict-mcp-config",
        "--no-session-persistence",
        "--max-budget-usd", "2",
    ]
    env = dict(os.environ)
    if arm == "stock":
        cmd += ["--mcp-config", str(scratch / "harness" / "mcp-empty.json")]
    else:
        rule = (scratch / "harness" / "reuse-rule.md").read_text()
        if arm == "rule":
            cmd += ["--mcp-config", str(scratch / "harness" / "mcp-empty.json")]
        elif arm in ("rule_cat_prompt", "rule_cat_file", "rule_cat_tree"):
            extra = {"rule_cat_prompt": "catalogue-compact.md", "rule_cat_file": "cat-file-hint.md", "rule_cat_tree": "tree-hint.md"}[arm]
            rule += "\n" + (scratch / "harness" / extra).read_text()
            cmd += ["--mcp-config", str(scratch / "harness" / "mcp-empty.json")]
        elif arm == "rule_skill":
            rule += "\n" + (scratch / "harness" / "skill-hint.md").read_text()
            cmd += ["--mcp-config", str(scratch / "harness" / "mcp-empty.json")]
        elif arm == "rule_lode":
            # Reuse rule + mandatory lodestone `search`, MCP pointed at the running daemon.
            rule += "\n" + (scratch / "harness" / "lode-must.md").read_text()
            cmd += ["--mcp-config", str(scratch / "harness" / "mcp-lode.json"),
                    "--allowedTools", "mcp__lodestone__search,mcp__lodestone__status"]
        elif arm in ("rule_ck", "rule_ck_must"):
            rule += "\n" + (scratch / "harness" / ("ck-must.md" if arm == "rule_ck_must" else "ck-hint.md")).read_text()
            cmd += ["--mcp-config", str(scratch / "harness" / "mcp-ck.json"), "--allowedTools", "mcp__ck"]
        else:
            raise SystemExit(f"unknown arm {arm}")
        cmd += ["--append-system-prompt", rule]
    with out.open("w") as fh:
        proc = subprocess.run(
            cmd, cwd=scratch / "trial-repo", stdout=fh, stderr=subprocess.PIPE, env=env, timeout=420
        )
    status = "ok" if proc.returncode == 0 else f"rc={proc.returncode} {proc.stderr.decode()[-200:]}"
    print(f"{arm} {item['id']} r{rep} {status}", flush=True)
    subprocess.run(["pkill", "-f", "tools/bin/ck --serve"], check=False)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("scratch", type=Path)
    ap.add_argument("--arms", default="stock,rule,rule_ck")
    ap.add_argument("--model", default="sonnet")
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--only", default="")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--file", default="questions.json")
    ap.add_argument("--runs", default="runs")
    args = ap.parse_args()

    items = json.loads((args.scratch / args.file).read_text())
    if args.only:
        keep = set(args.only.split(","))
        items = [i for i in items if i["id"] in keep]
    jobs = [(arm, item, rep) for rep in range(1, args.reps + 1) for item in items for arm in args.arms.split(",")]
    with ThreadPoolExecutor(args.jobs) as pool:
        futures = [pool.submit(run_one, args.scratch, arm, item, args.model, rep, args.runs) for arm, item, rep in jobs]
        for fut in futures:
            try:
                fut.result()
            except Exception as exc:  # keep going; a failed cell shows up as missing in scoring
                print(f"error: {exc}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    main()
