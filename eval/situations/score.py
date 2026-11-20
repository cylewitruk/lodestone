#!/usr/bin/env python3
"""Score the situations experiment.

Usage: score.py <out-dir> [--detail]

Prints one row per (situation, arm, task kind) and per (situation, arm): correctness, how often
lodestone and its skill were used, query form, and token/cost/time medians.
"""
import json
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from run import tasks  # noqa: E402

SEARCHES = {f"mcp__lodestone__{t}" for t in ("search", "semantic_code_search", "similar_code_search")}
SIMILAR = "mcp__lodestone__similar_code_search"
# Programs that search files (`git grep` too); as in `src/shell.rs`.
SEARCH_PROGRAMS = {"rg", "grep", "egrep", "fgrep", "ag", "ack"}
# An identifier-like word: snake_case with an underscore, or camel/Pascal case with an inner capital.
IDENT = re.compile(r"\b(?:[a-z][a-z0-9]*_[a-z0-9_]+|[a-z]+[A-Z]\w*|[A-Z][a-z0-9]+[A-Z]\w*)\b")


def query(i: dict) -> str:
    """A search call's text: the query and guessed names, or the code."""
    if "code" in i:
        code = i["code"]
        return "\n\n".join(code) if isinstance(code, list) else code
    return " ".join([i.get("query", ""), *i.get("example_identifiers", [])])


def first_words(command: str) -> list[list[str]]:
    """The words of each shell statement that does not read a pipe. Operators (`;`, `&`, `|`,
    `&&`, `||`, a newline, `<<`) and comments count only outside quotes; here-document bodies
    are skipped, and a line ending in `|`, `&&`, `||` or `\\` continues on the next."""
    out: list[list[str]] = []
    words: list[str] = []
    word: str | None = None
    piped = False  # the statement reads a pipe
    continued = False  # the last operator needs a command after it, so a newline continues
    heredoc: bool | None = None  # the next word names a here-document (True: `<<-`)
    delimiters: list[tuple[str, bool]] = []  # here-documents whose bodies follow this line
    quote = None
    i, n = 0, len(command)

    def end_word() -> None:
        nonlocal word, heredoc, continued
        if word is not None:
            if heredoc is not None:
                delimiters.append((word, heredoc))
                heredoc = None
            else:
                words.append(word)
                continued = False
            word = None

    def end_statement(op: str) -> None:
        nonlocal words, piped, continued
        end_word()
        if words and not piped:
            out.append(words)
        words, piped, continued = [], op == "|", op in ("|", "&&", "||")

    while i < n:
        c = command[i]
        if quote:
            if c == quote:
                quote = None
            elif c == "\\" and quote == '"' and i + 1 < n:
                i += 1
                word += command[i]
            else:
                word += c
            i += 1
        elif c in "'\"":
            quote, word = c, word or ""
            i += 1
        elif c == "\\" and command.startswith("\n", i + 1):  # line continuation
            i += 2
        elif c == "\\" and i + 1 < n:
            word = (word or "") + command[i + 1]
            i += 2
        elif c == "#" and word is None:  # a comment, to the end of the line
            nl = command.find("\n", i)
            i = n if nl < 0 else nl
        elif c in " \t":
            end_word()
            i += 1
        elif c == "\n":
            end_word()
            i += 1
            while delimiters and i < n:  # skip the bodies
                nl = command.find("\n", i)
                line, i = (command[i:], n) if nl < 0 else (command[i:nl], nl + 1)
                delimiter, tabs = delimiters[0]
                if (line.lstrip("\t") if tabs else line) == delimiter:
                    delimiters.pop(0)
            if not continued:
                end_statement("\n")
        elif command.startswith(("&&", "||"), i):
            end_statement(command[i : i + 2])
            i += 2
        elif command.startswith("<<<", i):  # a here-string: the next word is input
            end_word()
            i += 3
        elif command.startswith("<<", i):
            end_word()
            heredoc = command.startswith("<<-", i)
            i += 3 if heredoc else 2
        elif c == "&" and word and word.endswith((">", "<")):  # `2>&1`
            word += c
            i += 1
        elif c in ";&|":
            end_statement(c)
            i += 1
        else:
            word = (word or "") + c
            i += 1
    if quote is None:
        end_statement("")
    return out


def is_grep(name: str, i: dict) -> bool:
    """A text search: the Grep tool, or a shell command with a statement that starts one."""
    if name == "Grep":
        return True
    if name != "Bash":
        return False
    for words in first_words(i.get("command", "")):
        words = [w for w in words if not re.fullmatch(r"\w+=.*", w)][:2]
        program = words[0].rsplit("/", 1)[-1] if words else ""
        if program in SEARCH_PROGRAMS or (program == "git" and words[1:] == ["grep"]):
            return True
    return False


def combined(query: str) -> bool:
    return len(query.split()) >= 5 and bool(IDENT.search(query))


def parse(path: Path) -> dict:
    calls, text, result = [], "", None
    for line in path.read_text().splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "assistant":
            for b in ev.get("message", {}).get("content", []):
                if b.get("type") == "tool_use":
                    calls.append((b["name"], b.get("input", {})))
                elif b.get("type") == "text":
                    text = b["text"]
        elif ev.get("type") == "result":
            result = ev
    r = result or {}
    usage = r.get("usage", {})
    names = [n for n, _ in calls]
    queries = [query(i) for n, i in calls if n in SEARCHES]
    first_search = next((k for k, n in enumerate(names) if n in SEARCHES), None)
    greps = [k for k, (n, i) in enumerate(calls) if is_grep(n, i)]
    return {
        "final": r.get("result") or text or "",
        "complete": result is not None and not r.get("is_error"),
        "tokens": sum(usage.get(k, 0) for k in (
            "input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")),
        "cost": r.get("total_cost_usd") or 0.0,
        "secs": (r.get("duration_ms") or 0) / 1000,
        "turns": r.get("num_turns") or 0,
        "calls": len(calls),
        "searches": len(queries),
        "greps": len(greps),
        "reads": names.count("Read"),
        "globs": names.count("Glob"),
        "skill": any(n == "Skill" and "lodestone" in json.dumps(i) for n, i in calls),
        "combined": sum(map(combined, queries)) / len(queries) if queries else None,
        "similar": names.count(SIMILAR),
        "grep_first": bool(greps) and (first_search is None or greps[0] < first_search),
        "queries": queries,
    }


def named(final: str, tag: str) -> list[str]:
    return [m.strip().strip("`") for m in re.findall(rf"{tag}:\s*(.+)", final)]


def strip_comments(code: str) -> str:
    return re.sub(r"/\*.*?\*/", "", re.sub(r"//[^\n]*", "", code), flags=re.S)


def grade(situation: str, t: dict, final: str) -> bool | None:
    """True/False, or None when the task needs a human (free-form controls)."""
    situation = t.get("situation", situation)
    if situation == "write":
        code = "\n".join(re.findall(r"```(?:\w+)?\n(.*?)```", final, re.S)) or final
        return bool(re.search(t["accept"], strip_comments(code)))
    if "prompt" in t and situation in ("review", "refactor"):
        # Free-form replies have no answer line: the target must be mentioned; controls are hand-graded.
        if not t["answers"]:
            return None
        return any(hit(final, a) for a in t["answers"])
    tag = {"review": "REUSE", "refactor": "SAME", "locate": "ANSWER"}[situation]
    lines = named(final, tag)
    if situation == "locate":
        lines = lines[-1:]
    if not t["answers"]:
        return bool(lines) and all(l.lower().startswith("none") for l in lines)
    target = t.get("target", {}).get("symbol")
    return any(hit(l, a, a["symbol"] == target) for l in lines for a in t["answers"])


def hit(line: str, a: dict, need_file: bool = False) -> bool:
    """The answer's name, plus its file name when the name alone is ambiguous: short, or shared
    with the refactor target."""
    if not re.search(rf"\b{re.escape(a['symbol'])}\b", line):
        return False
    if len(a["symbol"]) < 8 or need_file:
        return not a.get("path") or Path(a["path"]).name in line
    return True


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else float("nan")


def pct(xs):
    return f"{100 * sum(xs) / len(xs):.0f}%" if xs else "—"


def row(label: str, runs: list[dict]) -> str:
    correct = [r["correct"] for r in runs]
    cost = sum(r["cost"] for r in runs)
    comb = [r["combined"] for r in runs if r["combined"] is not None]
    return (f"| {label} | {len(runs)} | {sum(correct)}/{len(runs)} | {pct([r['searches'] > 0 for r in runs])} "
            f"| {pct([r['skill'] for r in runs])} | {med([r['searches'] for r in runs]):.0f} "
            f"| {f'{100 * med(comb):.0f}%' if comb else '—'} | {med([r['tokens'] for r in runs]) / 1000:.1f}k "
            f"| {med([r['secs'] for r in runs]):.0f} | {med([r['calls'] for r in runs]):.0f} "
            f"| {med([r['greps'] for r in runs]):.0f} | ${cost:.2f} "
            f"| {'$%.3f' % (cost / sum(correct)) if sum(correct) else '—'} |")


def main() -> None:
    out = Path(sys.argv[1])
    detail = "--detail" in sys.argv
    print("| situation / arm / kind | runs | correct | searched | skill | med searches | combined queries "
          "| med tokens | med secs | med calls | med greps | cost | cost/correct |")
    print("| ---- " * 13 + "|")
    dump = []
    for sdir in sorted(p for p in out.iterdir() if p.is_dir()):
        situation = sdir.name
        items = {t["id"]: t for t in tasks(situation)}
        for adir in sorted(p for p in sdir.iterdir() if p.is_dir()):
            by_kind = defaultdict(list)
            for path in sorted(adir.glob("*.jsonl")):
                t = items[path.name.split(".")[0]]
                r = parse(path)
                g = grade(situation, t, r["final"]) if r["complete"] else False
                r["correct"], r["needs_hand"] = bool(g), g is None
                kind = f"{t['situation']}-{t['kind']}" if "situation" in t else t["kind"]
                r.update(id=t["id"], kind=kind, situation=situation, arm=adir.name)
                by_kind[kind].append(r)
                dump.append({k: v for k, v in r.items() if k != "final"})
            all_runs = [r for rs in by_kind.values() for r in rs]
            print(row(f"{situation} / {adir.name}", all_runs))
            if len(by_kind) > 1:
                for kind, rs in sorted(by_kind.items()):
                    print(row(f"{situation} / {adir.name} / {kind}", rs))
            if detail:
                for r in all_runs:
                    if r["queries"]:
                        print(f"<!-- {r['id']} {adir.name}: {r['queries']} -->")
    (out / "scores.json").write_text(json.dumps(dump, indent=1))


if __name__ == "__main__":
    main()
