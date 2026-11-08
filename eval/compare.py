#!/usr/bin/env python3
"""Compare `lode bench --json` reports.

Usage: compare.py <reports-dir> [--baseline LABEL]

Report files are named `<label>-<corpus>.json` (e.g. `emb-coderank-off-stacks.json` is
accepted as label `emb-coderank-off`, corpus `stacks`; the runner's
`emb-<model>-<corpus>-<off|on>.json` naming is normalised too). For every label present on
all corpora it prints pooled metrics over the answerable questions of every corpus, and,
against the baseline, the per-question wins/losses for "answer in the agent's top 8" plus a
bootstrap 95% interval for the change in top-8 rate and MRR.
"""

import json
import random
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path

CORPORA = ("stacks", "tantivy", "librechat")
TOP_K = 8


def parse_name(stem):
    m = re.match(r"^(emb-.+)-(stacks|tantivy|librechat)-(off|on)$", stem)
    if m:
        return f"{m.group(1)}-{m.group(3)}", m.group(2)
    m = re.match(r"^(.+)-(stacks|tantivy|librechat)$", stem)
    if m:
        return m.group(1), m.group(2)
    return None, None


def load(dir_):
    runs = defaultdict(dict)
    for p in Path(dir_).glob("*.json"):
        label, corpus = parse_name(p.stem)
        if label:
            runs[label][corpus] = json.loads(p.read_text())
    return {k: v for k, v in runs.items() if all(c in v for c in CORPORA)}


def ranks(run):
    """{(corpus, id): rank or None} over answerable questions."""
    out = {}
    for corpus, rep in run.items():
        for q in rep["questions"]:
            if q["kind"] != "negative":
                out[(corpus, q["id"])] = q["rank"]
    return out


def hit(r, k=TOP_K):
    return 1.0 if r is not None and r <= k else 0.0


def rr(r):
    return 1.0 / r if r is not None else 0.0


def summary(run):
    rk = ranks(run)
    n = len(rk)
    idx = [rep.get("cold_index") for rep in run.values()]
    secs = sum(i["secs"] for i in idx if i) if all(idx) else None
    embedded = sum(i["chunks_embedded"] for i in idx if i) if all(idx) else None
    return {
        "n": n,
        "top8": sum(hit(r) for r in rk.values()) / n,
        "top5": sum(hit(r, 5) for r in rk.values()) / n,
        "mrr": sum(rr(r) for r in rk.values()) / n,
        "recall50": sum(hit(r, 50) for r in rk.values()) / n,
        "per_corpus_top8": {
            c: sum(hit(r) for (cc, _), r in rk.items() if cc == c)
            / max(1, sum(1 for (cc, _) in rk if cc == c))
            for c in CORPORA
        },
        "index_secs": secs,
        "chunks_per_sec": embedded / secs if secs else None,
        "p50_ms": statistics.median(rep["latency_p50_ms"] for rep in run.values()),
    }


def paired(base, other, iters=4000, seed=7):
    b, o = ranks(base), ranks(other)
    keys = sorted(set(b) & set(o))
    wins = sum(1 for k in keys if hit(o[k]) > hit(b[k]))
    losses = sum(1 for k in keys if hit(o[k]) < hit(b[k]))
    d_top8 = [hit(o[k]) - hit(b[k]) for k in keys]
    d_mrr = [rr(o[k]) - rr(b[k]) for k in keys]
    rnd = random.Random(seed)

    def ci(d):
        means = sorted(
            sum(rnd.choice(d) for _ in d) / len(d) for _ in range(iters)
        )
        return means[int(0.025 * iters)], means[int(0.975 * iters)]

    return {
        "wins": wins,
        "losses": losses,
        "d_top8": sum(d_top8) / len(keys),
        "ci_top8": ci(d_top8),
        "d_mrr": sum(d_mrr) / len(keys),
        "ci_mrr": ci(d_mrr),
    }


def fmt(x, nd=3):
    return "—" if x is None else f"{x:.{nd}f}"


def main():
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    baseline = "emb-coderank-off"
    if "--baseline" in args:
        i = args.index("--baseline")
        baseline = args[i + 1]
        del args[i : i + 2]
    runs = load(args[0])
    if baseline not in runs:
        sys.exit(f"baseline {baseline} missing; have {sorted(runs)}")
    base = runs[baseline]
    print(
        "| Run | n | Top 8 | rank@5 | MRR | Recall@50 | stacks / tantivy / librechat top 8 "
        "| Index s | Chunks/s | p50 ms | Top 8 vs baseline (wins/losses, Δ, 95% CI) | ΔMRR (95% CI) |"
    )
    print("| ---- " * 12 + "|")
    for label in sorted(runs, key=lambda l: -summary(runs[l])["top8"]):
        s = summary(runs[label])
        pc = " / ".join(fmt(s["per_corpus_top8"][c], 2) for c in CORPORA)
        if label == baseline:
            vs, dm = "baseline", "—"
        else:
            p = paired(base, runs[label])
            vs = (
                f"{p['wins']}/{p['losses']}, {p['d_top8']:+.3f} "
                f"[{p['ci_top8'][0]:+.3f}, {p['ci_top8'][1]:+.3f}]"
            )
            dm = f"{p['d_mrr']:+.3f} [{p['ci_mrr'][0]:+.3f}, {p['ci_mrr'][1]:+.3f}]"
        print(
            f"| {label} | {s['n']} | {fmt(s['top8'])} | {fmt(s['top5'])} | {fmt(s['mrr'])} "
            f"| {fmt(s['recall50'])} | {pc} | {fmt(s['index_secs'], 0)} "
            f"| {fmt(s['chunks_per_sec'], 1)} | {fmt(s['p50_ms'], 1)} | {vs} | {dm} |"
        )


if __name__ == "__main__":
    main()
