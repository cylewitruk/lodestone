#!/usr/bin/env python3
"""Run the situations experiment: one headless `claude -p` run per (situation, arm, task, rep).

Usage: run.py <out-dir> --repo <corpus> --port <daemon port>
              [--situations write,review,refactor,locate] [--arms none,tools,skill]
              [--reps 3] [--jobs 4] [--model sonnet] [--effort medium] [--subagents]
              [--only id,id]

Transcripts land in <out-dir>/<situation>/<arm>/<id>.r<rep>.jsonl; existing ones are skipped.
"""
import argparse
import json
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
EVAL = HERE.parent
# The shipped plugin (skill, MCP config, SessionStart reminder) is arm `hook`; arm `skill` is the
# same plugin without the reminder.
PLUGIN = EVAL.parent / "plugin"
PLUGIN_SKILL_ONLY = HERE / "plugin-skill"
TOOLS = "Read,Grep,Glob,Skill"
# Both generations of lodestone tools: `search`, and the semantic/similar pair that replaced it.
LODE_TOOLS = ",".join(f"mcp__lodestone__{t}" for t in (
    "search", "semantic_code_search", "similar_code_search", "status"))
# `--setting-sources project` still loads the user's own CLAUDE.md; keep it out of the runs.
SETTINGS = json.dumps({"claudeMdExcludes": [str(Path.home() / ".claude" / "CLAUDE.md")]})

WRITE_SUFFIX = (
    "\n\nReply with the complete Rust code in one fenced block (include the `use` lines it needs). "
    "Do not edit any files."
)
REVIEW = (
    "Review this change to the repository before it is merged and point out problems briefly. "
    "Do not edit any files.\n\n```diff\n{diff}\n```\n\n"
    "End your reply with one line for each existing definition in the repository that the change "
    "should reuse instead of the code it adds, in exactly this form: "
    "`REUSE: <repo-relative path>::<name>`, or a single line `REUSE: none`."
)
REFACTOR = (
    "We are consolidating duplicated code. Look at `{symbol}` in `{path}` (line {line}). If one "
    "or more other functions elsewhere in this repository do the same job, so they could be merged, "
    "name them. Do not edit any files.\n\n"
    "End your reply with one line for each equivalent in exactly this form: "
    "`SAME: <repo-relative path>::<name>`, or a single line `SAME: none`."
)
LOCATE_SUFFIX = (
    "\n\nFind the code responsible and answer briefly. Do not edit any files. End your reply with "
    "one final line in exactly this form: `ANSWER: <repo-relative path>::<function name>`."
)


def tasks(situation: str) -> list[dict]:
    """`<situation>.json` here, or in `$SITUATIONS_TASKS` (task sets kept out of the repository,
    e.g. tasks on private code)."""
    path = EVAL / "tasks.json" if situation == "write" else HERE / f"{situation}.json"
    outside = Path(os.environ.get("SITUATIONS_TASKS", "")) / f"{situation}.json"
    if not path.exists() and os.environ.get("SITUATIONS_TASKS") and outside.exists():
        path = outside
    return json.loads(path.read_text())


def prompt(situation: str, t: dict) -> str:
    if "prompt" in t:  # free-form tasks (pilot.json) carry their full prompt
        return t["prompt"]
    if situation == "write":
        return t["question"] + WRITE_SUFFIX
    if situation == "review":
        return REVIEW.format(diff=t["diff"])
    if situation == "refactor":
        return REFACTOR.format(**t["target"])
    return t["question"] + LOCATE_SUFFIX


def run_one(args, situation: str, arm: str, t: dict, rep: int) -> None:
    out = args.out / situation / arm / f"{t['id']}.r{rep}.jsonl"
    if out.exists() and out.stat().st_size > 0:
        return
    out.parent.mkdir(parents=True, exist_ok=True)
    lodestone = arm not in ("none", "remind")
    cfg = args.out / (f"mcp-lode-{args.port}.json" if lodestone else "mcp-empty.json")
    cmd = [
        "claude", "-p", prompt(situation, t),
        "--model", args.model,
        "--output-format", "stream-json", "--verbose",
        "--tools", TOOLS + (",Agent" if args.subagents else ""),
        "--setting-sources", "project",
        "--settings", SETTINGS,
        "--strict-mcp-config", "--mcp-config", str(cfg),
        "--no-session-persistence",
        "--max-budget-usd", "2",
    ]
    if args.effort:
        cmd += ["--effort", args.effort]
    if lodestone:
        cmd += ["--allowedTools", LODE_TOOLS]
    if arm == "skill":
        cmd += ["--plugin-dir", str(PLUGIN_SKILL_ONLY)]
    elif arm == "hook":
        cmd += ["--plugin-dir", str(PLUGIN)]
    elif arm == "shipped":  # the current plugin (or `--plugin`); `hook` was recorded with an earlier description
        cmd += ["--plugin-dir", str(args.plugin)]
    elif arm == "remind":  # control: the reminder's advice without lodestone
        cmd += ["--plugin-dir", str(HERE / "plugin-remind")]
    elif arm.startswith("skill-"):  # skill-description variants, e.g. skill-v2
        cmd += ["--plugin-dir", str(HERE / "skill-variants" / arm.removeprefix("skill-"))]
    tmp = out.with_suffix(".tmp")
    env = dict(os.environ, LODESTONE_PORT=str(args.port))  # the plugin's hook checks this daemon
    with tmp.open("w") as fh:
        proc = subprocess.run(cmd, cwd=args.repo, stdout=fh, stderr=subprocess.PIPE, env=env, timeout=600)
    tmp.rename(out)
    status = "ok" if proc.returncode == 0 else f"rc={proc.returncode} {proc.stderr.decode()[-200:]}"
    print(f"{situation} {arm} {t['id']} r{rep} {status}", flush=True)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("out", type=Path)
    ap.add_argument("--repo", type=Path, required=True)
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--situations", default="write,review,refactor,locate")
    ap.add_argument("--arms", default="none,tools,skill")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--model", default="sonnet")
    ap.add_argument("--effort", default="", help="default: the model's Claude Code default")
    ap.add_argument("--subagents", action="store_true", help="also allow the Agent tool (e.g. Explore)")
    ap.add_argument("--only", default="")
    ap.add_argument("--plugin", type=Path, default=PLUGIN, help="plugin directory for arm `shipped`")
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "mcp-empty.json").write_text('{"mcpServers": {}}\n')
    # Per port: concurrent runs against different daemons share `out`.
    (args.out / f"mcp-lode-{args.port}.json").write_text(json.dumps(
        {"mcpServers": {"lodestone": {"type": "http", "url": f"http://127.0.0.1:{args.port}/mcp"}}}))
    keep = set(filter(None, args.only.split(",")))
    jobs = [
        (s, arm, t, rep)
        for rep in range(1, args.reps + 1)
        for s in args.situations.split(",")
        for t in tasks(s)
        if not keep or t["id"] in keep
        for arm in args.arms.split(",")
    ]
    with ThreadPoolExecutor(args.jobs) as pool:
        for fut in [pool.submit(run_one, args, *j) for j in jobs]:
            try:
                fut.result()
            except Exception as exc:  # a failed cell shows up as missing when scoring
                print(f"error: {exc}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    main()
