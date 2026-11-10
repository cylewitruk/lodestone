#!/usr/bin/env python3
"""Builds per-style question files from the rewrites in `<corpus>.json`.

Usage: build.py <out-dir> [--hard]

Writes `<out-dir>/<corpus>-<style>.json` for every style: the corpus's question file with each
answerable question's text replaced by its rewrite; negative questions are kept as they are.
`--hard` builds the hard set instead (`questions-hard-<corpus>.json`): `hard-<corpus>-<style>.json`
from `hard-<corpus>.json`, written without using any memory of the repository, and
`hard-recall-<corpus>-combined.json` from `hard-recall-<corpus>.json`, whose writer was told the
repository's name and recalled many of its real identifiers.
The rewrites are written blind (from the question text only); the per-style count of rewrites
that name an answer symbol anyway is printed for reference. `combined` is derived, not written:
the `task` rewrite followed by the `identifier` rewrite on a new line. `redacted` is the
`identifier` rewrite without the words that contain an answer symbol (a worst case for guessing);
`combined-redacted` does the same to both halves of `combined`.
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
STYLES = ("terse", "short", "task", "identifier", "combined", "redacted", "combined-redacted")
HARD_STYLES = ("task", "identifier", "combined", "combined-redacted")
# set -> (question file per corpus, [(rewrite-file prefix, styles)])
SETS = {
    "default": (SOURCES, [("", STYLES)]),
    "hard": (
        {c: f"questions-hard-{c}.json" for c in SOURCES},
        [("hard-", HARD_STYLES), ("hard-recall-", ("combined",))],
    ),
}


def redact(text, symbols):
    return " ".join(w for w in text.split() if not any(s in w.lower() for s in symbols))


def rewrite(r, style, symbols):
    if style == "combined":
        return f"{r['task']}\n{r['identifier']}"
    if style == "redacted":
        return redact(r["identifier"], symbols)
    if style == "combined-redacted":
        return f"{redact(r['task'], symbols)}\n{redact(r['identifier'], symbols)}"
    return r[style]


def main(out, which):
    out.mkdir(parents=True, exist_ok=True)
    sources, groups = SETS[which]
    named = Counter()
    written = 0
    for corpus, source in sources.items():
        questions = json.loads((EVAL / source).read_text())
        for prefix, styles in groups:
            rewrites = json.loads((HERE / f"{prefix}{corpus}.json").read_text())
            for style in styles:
                built = []
                for q in questions:
                    q = dict(q)
                    if q.get("answers"):
                        symbols = [a["symbol"].lower() for a in q["answers"]]
                        text = rewrite(rewrites[q["id"]], style, symbols)
                        if any(s in text.lower() for s in symbols):
                            named[prefix + style] += 1
                        q["question"] = text
                    built.append(q)
                (out / f"{prefix}{corpus}-{style}.json").write_text(json.dumps(built, indent=1))
                written += 1
    print(f"wrote {written} files to {out}")
    print("rewrites naming an answer symbol:", dict(named))


if __name__ == "__main__":
    main(Path(sys.argv[1]), "hard" if "--hard" in sys.argv[2:] else "default")
