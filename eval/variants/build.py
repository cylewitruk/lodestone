#!/usr/bin/env python3
"""Builds per-style question files from the rewrites in `<corpus>.json`.

Usage: build.py <out-dir>

Writes `<out-dir>/<corpus>-<style>.json` for every style: the corpus's question file with each
answerable question's text replaced by its rewrite; negative questions are kept as they are.
The rewrites are written blind (from the question text only); the per-style count of rewrites
that name an answer symbol anyway is printed for reference.
"""

import json
import sys
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent
EVAL = HERE.parent
SOURCES = {
    "stacks": "questions.json",
    "tantivy": "questions-tantivy.json",
    "librechat": "questions-librechat.json",
}
STYLES = ("terse", "short", "task", "identifier")


def main(out):
    out.mkdir(parents=True, exist_ok=True)
    named = Counter()
    for corpus, source in SOURCES.items():
        questions = json.loads((EVAL / source).read_text())
        rewrites = json.loads((HERE / f"{corpus}.json").read_text())
        for style in STYLES:
            built = []
            for q in questions:
                q = dict(q)
                if q.get("answers"):
                    text = rewrites[q["id"]][style]
                    if any(a["symbol"].lower() in text.lower() for a in q["answers"]):
                        named[style] += 1
                    q["question"] = text
                built.append(q)
            (out / f"{corpus}-{style}.json").write_text(json.dumps(built, indent=1))
    print(f"wrote {len(SOURCES) * len(STYLES)} files to {out}")
    print("rewrites naming an answer symbol:", dict(named))


if __name__ == "__main__":
    main(Path(sys.argv[1]))
