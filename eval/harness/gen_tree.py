#!/usr/bin/env python3
"""Generate a hierarchical code directory under <repo>/docs/catalogue/.

README.md: crates -> top-level modules with one-liners (small; meant for the prompt).
<crate>/<dir>.md: per directory, each file's one-liner + pub items, links to subdirectories.

Usage: gen_tree.py <repo> [out_dir]
"""
import re
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen_catalogue import scan  # noqa: E402

SKIP_DIRS = {"target", ".ck", ".git", "node_modules"}


def one_liner(path: Path, items: list[str]) -> str:
    for line in path.read_text(errors="replace").splitlines()[:40]:
        s = line.strip()
        if s.startswith("//!"):
            t = s[3:].strip()
            if t:
                return t[:110]
        elif s and not s.startswith("//") and not s.startswith("#!"):
            break
    names = re.findall(r"`(?:fn )?([A-Za-z_][A-Za-z0-9_]*)", "\n".join(items))
    names = [n for n in dict.fromkeys(names) if n not in ("fn",)]
    return ("defines " + ", ".join(names[:6])) if names else ""


def crates(repo: Path) -> list[Path]:
    found = []
    for toml in repo.glob("*/Cargo.toml"):
        if (toml.parent / "src").is_dir():
            found.append(toml.parent)
    for toml in repo.glob("*/*/Cargo.toml"):
        if toml.parent.parts[-2] not in SKIP_DIRS and (toml.parent / "src").is_dir():
            found.append(toml.parent)
    return sorted(found)


def crate_desc(crate: Path) -> str:
    m = re.search(r'^description\s*=\s*"([^"]+)"', (crate / "Cargo.toml").read_text(), re.M)
    if m:
        return m.group(1)[:110]
    for lib in (crate / "src" / "lib.rs", crate / "src" / "main.rs"):
        if lib.exists():
            return one_liner(lib, [])
    return ""


def main() -> None:
    repo = Path(sys.argv[1]).resolve()
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else repo / "docs" / "catalogue"
    out.mkdir(parents=True, exist_ok=True)
    readme = ["# Code directory", "",
              "Crates (lowest layer first; a crate can only reuse from crates listed before it). Each entry links to a "
              "page listing that directory's files, their `pub` items (signature — first doc line) and subdirectories. "
              "Descend to the crate and module that would own a concept before writing a new helper.", ""]
    total_items = 0
    for crate in crates(repo):
        rel_crate = crate.relative_to(repo)
        src = crate / "src"
        readme += [f"## {rel_crate} — {crate_desc(crate)}", ""]
        # group files by directory
        by_dir: dict[Path, list[Path]] = defaultdict(list)
        for p in sorted(src.rglob("*.rs")):
            if any(part in SKIP_DIRS for part in p.parts):
                continue
            by_dir[p.parent].append(p)
        for d in sorted(by_dir):
            rel_dir = d.relative_to(repo)
            page = out / f"{rel_dir}.md"
            page.parent.mkdir(parents=True, exist_ok=True)
            subdirs = sorted(x for x in by_dir if x.parent == d)
            lines = [f"# {rel_dir}", ""]
            if subdirs:
                lines += ["Subdirectories: " + ", ".join(f"[{x.name}/]({x.name}/{x.name}.md)" if False else f"`{x.relative_to(repo)}` (see `docs/catalogue/{x.relative_to(repo)}.md`)" for x in subdirs), ""]
            for p in by_dir[d]:
                items = scan(p, str(p.relative_to(repo)))
                total_items += len(items)
                lines += [f"## {p.relative_to(repo)} — {one_liner(p, items)}", ""] + (items or ["- (no pub items)"]) + [""]
            page.write_text("\n".join(lines))
            if d == src:
                # README lists the crate's top-level files and directories
                for p in by_dir[d]:
                    if p.name in ("lib.rs", "main.rs"):
                        continue
                    readme.append(f"- `{p.relative_to(crate)}` — {one_liner(p, scan(p, ''))}")
                for x in subdirs:
                    mod = x / "mod.rs"
                    desc = one_liner(mod, scan(mod, "")) if mod.exists() else "defines " + ", ".join(f.stem for f in by_dir[x][:5])
                    readme.append(f"- `{x.relative_to(crate)}/` — {desc}  → `docs/catalogue/{x.relative_to(repo)}.md`")
        readme.append("")
    (out / "README.md").write_text("\n".join(readme))
    print(f"README {len('\n'.join(readme))} bytes; {total_items} items across pages")


if __name__ == "__main__":
    main()
