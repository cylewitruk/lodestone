#!/usr/bin/env python3
"""Per-crate, importance-filtered helper slices.

For every workspace crate, emit <out>/<crate>.md listing the top-N `pub` items (free fns, types,
traits, macros, consts) visible from that crate — its own plus its transitive workspace deps — ranked
by the number of distinct files that reference the item. Methods are excluded to save budget.

Usage: gen_slices.py <repo> <out_dir> [top_n]
"""
import json
import re
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen_catalogue import scan  # noqa: E402
from gen_tree import SKIP_DIRS, crates  # noqa: E402

IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
NAME_IN_ITEM = re.compile(r"^- (?:macro `([A-Za-z0-9_]+)!`|(?:struct|enum|trait|type|const|static) `([A-Za-z0-9_]+)`|`(?:fn )?([A-Za-z0-9_]+)\b)")
NOISE = {"new", "len", "from", "default", "fmt", "get", "set", "next", "into", "clone", "iter", "true", "false", "ref", "Error", "Result", "Option"}


def crate_deps(repo: Path, crate_dirs: list[Path]) -> dict[str, set[str]]:
    """crate dir name -> workspace crate dir names it depends on (incl. dev-deps), transitively."""
    root = (repo / "Cargo.toml").read_text()
    ws_paths = dict(re.findall(r'^([A-Za-z0-9_-]+)\s*=\s*\{[^}]*path\s*=\s*"([^"]+)"', root, re.M))
    by_dir = {c.name: c for c in crate_dirs}
    direct: dict[str, set[str]] = {}
    for c in crate_dirs:
        toml = (c / "Cargo.toml").read_text()
        deps = set()
        for name, rest in re.findall(r"^([A-Za-z0-9_-]+)\s*=\s*(\{[^}]*\}|\"[^\"]*\")", toml, re.M):
            m = re.search(r'path\s*=\s*"([^"]+)"', rest)
            target = (c / m.group(1)).resolve().name if m else ((repo / ws_paths[name]).resolve().name if "workspace" in rest and name in ws_paths else None)
            if target in by_dir and target != c.name:
                deps.add(target)
        direct[c.name] = deps
    trans: dict[str, set[str]] = {}
    def walk(n, seen):
        for d in direct.get(n, ()):
            if d not in seen:
                seen.add(d); walk(d, seen)
        return seen
    for n in direct:
        trans[n] = walk(n, set())
    return trans


def main() -> None:
    repo = Path(sys.argv[1]).resolve()
    out = Path(sys.argv[2]); out.mkdir(parents=True, exist_ok=True)
    top_n = int(sys.argv[3]) if len(sys.argv) > 3 else 400
    crate_dirs = crates(repo)
    deps = crate_deps(repo, crate_dirs)

    # 1. all items, by crate
    items: list[dict] = []
    files: list[Path] = []
    for c in crate_dirs:
        for p in sorted((c / "src").rglob("*.rs")):
            if any(part in SKIP_DIRS for part in p.parts):
                continue
            files.append(p)
            for line in scan(p, ""):
                if "::fn " in line:
                    continue  # methods
                m = NAME_IN_ITEM.match(line)
                if not m:
                    continue
                name = next(g for g in m.groups() if g)
                if name in NOISE or len(name) < 3:
                    continue
                items.append({"crate": c.name, "file": str(p.relative_to(repo)), "name": name, "line": line, "doc": " — " in line})

    # 2. identifiers per file -> reference counts (distinct files other than the defining one)
    idents = {str(p.relative_to(repo)): set(IDENT.findall(p.read_text(errors="replace"))) for p in files}
    by_name: dict[str, set[str]] = defaultdict(set)
    for f, ids in idents.items():
        for i in ids:
            by_name[i].add(f)
    for it in items:
        it["refs"] = len(by_name.get(it["name"], set()) - {it["file"]})
        it["score"] = it["refs"] + (3 if it["doc"] else 0)

    # 3. per-crate slices
    (out / "index.json").write_text(json.dumps(items))
    sizes = {}
    for c in crate_dirs:
        visible = {c.name} | deps.get(c.name, set())
        own = sorted((it for it in items if it["crate"] == c.name and it["refs"] >= 1), key=lambda it: (-it["score"], it["file"], it["name"]))
        below = sorted((it for it in items if it["crate"] in visible - {c.name} and it["refs"] >= 2), key=lambda it: (-it["score"], it["file"], it["name"]))
        own_n = min(len(own), top_n // 2 + max(0, top_n // 2 - len(below)))
        chosen = own[:own_n] + below[:top_n - own_n]
        pool = own + below
        grouped: dict[str, list[dict]] = defaultdict(list)
        for it in chosen:
            grouped[it["file"]].append(it)
        layers = ", ".join(sorted(visible))
        lines = [f"# Reusable helpers visible from `{c.name}`", "",
                 f"The {len(chosen)} most-referenced `pub` items (of {len(pool)} candidates; this crate's own first) across "
                 f"{layers}. One line per item: signature — first doc line. Methods and rarely-used items are "
                 "omitted: if nothing here fits, grep the crates listed above before writing a new helper.", ""]
        for f in sorted(grouped, key=lambda f: (-max(i["score"] for i in grouped[f]), f)):
            lines += [f"## {f}", ""] + [it["line"] for it in grouped[f]] + [""]
        text = "\n".join(lines)
        (out / f"{c.name}.md").write_text(text)
        sizes[c.name] = (len(chosen), len(pool), len(text) // 4)
    for k, v in sorted(sizes.items()):
        print(f"{k:<24} items={v[0]:<4} pool={v[1]:<5} ~tokens={v[2]}")


if __name__ == "__main__":
    main()
