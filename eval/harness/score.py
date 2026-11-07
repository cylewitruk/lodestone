#!/usr/bin/env python3
"""Score pilot runs. Usage: score.py <scratchpad> [--detail]"""
import json
import re
import sys
from collections import defaultdict
from pathlib import Path


def parse_run(path: Path) -> dict:
    tools, result, text = defaultdict(int), None, ""
    for line in path.read_text().splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "assistant":
            for block in ev.get("message", {}).get("content", []):
                if block.get("type") == "tool_use":
                    tools[block["name"]] += 1
                elif block.get("type") == "text":
                    text = block["text"]
        elif ev.get("type") == "result":
            result = ev
    final = (result or {}).get("result") or text
    usage = (result or {}).get("usage", {})
    tokens = sum(usage.get(k, 0) for k in (
        "input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"))
    return {
        "final": final or "",
        "tools": dict(tools),
        "turns": (result or {}).get("num_turns"),
        "cost": (result or {}).get("total_cost_usd"),
        "tokens": tokens,
        "secs": ((result or {}).get("duration_ms") or 0) / 1000,
        "complete": result is not None and not (result or {}).get("is_error"),
    }


def grade(item: dict, final: str) -> bool:
    if item["kind"] == "task":
        code = "\n".join(re.findall(r"```(?:rust)?\n(.*?)```", final, re.S)) or final
        return bool(re.search(item["accept"], code))
    m = re.findall(r"ANSWER:\s*(.+)", final)
    answer = m[-1].strip().strip("`") if m else ""
    if item["kind"] == "negative":
        return answer.lower().startswith("none")
    return any(re.search(rf"\b{re.escape(a['symbol'])}\b", answer) for a in item["answers"])


def main() -> None:
    scratch = Path(sys.argv[1])
    detail = "--detail" in sys.argv
    args = [a for a in sys.argv[2:] if not a.startswith("--")]
    qfile, runs = (args + ["questions.json", "runs"])[:2] if len(args) < 2 else args[:2]
    items = json.loads((scratch / qfile).read_text())
    arms = sorted(p.name for p in (scratch / runs).iterdir() if p.is_dir())
    rows = defaultdict(lambda: defaultdict(list))
    for arm in arms:
        for item in items:
            for path in sorted((scratch / runs / arm).glob(f"{item['id']}.r*.jsonl")):
                run = parse_run(path)
                run["correct"] = run["complete"] and grade(item, run["final"])
                rows[item["id"]][arm].append(run)

    kinds = ["easy", "mismatch", "negative", "task", "all"]
    print(f"{'arm':<9} " + " ".join(f"{k:>9}" for k in kinds) + "   med_tok  med_tools  med_cost  med_secs  ck_runs  incomplete")
    for arm in arms:
        cells, toks, ntools, costs, secs, ck_runs, incomplete = defaultdict(list), [], [], [], [], 0, 0
        for item in items:
            for run in rows[item["id"]][arm]:
                cells[item["kind"]].append(run["correct"])
                cells["all"].append(run["correct"])
                incomplete += not run["complete"]
                toks.append(run["tokens"])
                ntools.append(sum(run["tools"].values()))
                costs.append(run["cost"] or 0)
                secs.append(run["secs"])
                ck_runs += any(t.startswith("mcp__ck") for t in run["tools"])
        med = lambda xs: sorted(xs)[len(xs) // 2] if xs else 0
        frac = lambda k: f"{sum(cells[k])}/{len(cells[k])}"
        print(f"{arm:<9} " + " ".join(f"{frac(k):>9}" for k in kinds)
              + f"  {med(toks):>8}  {med(ntools):>9}  {med(costs):>8.3f}  {med(secs):>8.0f}  {ck_runs:>7}  {incomplete:>10}")
        print(f"{'':<9} total cost ${sum(costs):.2f}")

    if detail:
        print()
        for item in items:
            marks = []
            for arm in arms:
                runs = rows[item["id"]][arm]
                answers = []
                for run in runs:
                    ans = re.findall(r"ANSWER:\s*(.+)", run["final"])
                    answers.append(("Y" if run["correct"] else "N") + f"({sum(run['tools'].values())}t)" + ("" if run["correct"] else " " + (ans[-1][:50] if ans else "?")))
                marks.append(f"{arm}: " + " | ".join(answers))
            want = ",".join(a["symbol"] for a in item["answers"]) or "none"
            print(f"{item['id']} [{item['kind']}] want={want}\n    " + "\n    ".join(marks))


if __name__ == "__main__":
    main()
