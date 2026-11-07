#!/usr/bin/env python3
"""Generate a helper catalogue: one line per `pub` item (signature + first doc line) grouped by file.

Usage: gen_catalogue.py <repo> <out.md> <glob> [<glob> ...]
"""
import re
import sys
from pathlib import Path

ITEM = re.compile(r"^(\s*)pub\s+(?:unsafe\s+)?(?:async\s+)?(?:const\s+)?(fn|struct|enum|trait|type|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)")
MACRO = re.compile(r"^\s*macro_rules!\s+([A-Za-z_][A-Za-z0-9_]*)")
IMPL = re.compile(r"^impl(?:<[^>]*>)?\s+(?:(?:[A-Za-z0-9_:<>, ]+)\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)")
TEST_MARK = re.compile(r"^\s*#\[(cfg\(test\)|test)\]")


def signature(lines: list[str], i: int) -> str:
    buf = []
    for line in lines[i:i + 12]:
        buf.append(line.strip())
        if "{" in line or line.rstrip().endswith(";"):
            break
    sig = " ".join(buf)
    sig = re.sub(r"\s*\{.*$", "", sig)
    sig = re.sub(r"\s+", " ", sig).replace("pub ", "", 1).rstrip(";")
    sig = re.sub(r"\s+where\s.*$", "", sig)
    return sig[:150]


def scan(path: Path, rel: str) -> list[str]:
    lines = path.read_text(errors="replace").splitlines()
    out, doc, impl_ty, macro_export = [], [], None, False
    for i, line in enumerate(lines):
        s = line.strip()
        if s.startswith("///"):
            doc.append(s[3:].strip())
            continue
        if s.startswith("#[macro_export]"):
            macro_export = True
            continue
        if m := IMPL.match(line):
            impl_ty = m.group(1)
        elif line.startswith("}"):
            impl_ty = None
        if m := MACRO.match(line):
            if macro_export:
                out.append(f"- macro `{m.group(1)}!`" + (f" — {doc[0]}" if doc else ""))
            macro_export = False
        elif m := ITEM.match(line):
            indent, kind, name = m.groups()
            if kind == "fn":
                sig = signature(lines, i)
                label = f"`{impl_ty}::{sig}`" if indent and impl_ty else f"`{sig}`"
            else:
                label = f"{kind} `{name}`"
            out.append(f"- {label}" + (f" — {doc[0]}" if doc else ""))
        if not s.startswith("#["):
            doc = []
    return out


def main() -> None:
    repo, out = Path(sys.argv[1]), Path(sys.argv[2])
    files = sorted({p for g in sys.argv[3:] for p in repo.glob(g) if p.suffix == ".rs"})
    parts = ["# Helper catalogue", "", "One line per `pub` item: signature — first doc line. Grouped by file; a crate can only reuse from layers below it.", ""]
    n = 0
    for p in files:
        rel = str(p.relative_to(repo))
        items = scan(p, rel)
        if not items:
            continue
        n += len(items)
        parts += [f"## {rel}", ""] + items + [""]
    out.write_text("\n".join(parts))
    print(f"{len(files)} files, {n} items, {out.stat().st_size} bytes")


if __name__ == "__main__":
    main()
