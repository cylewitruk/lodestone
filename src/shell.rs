//! What a shell command searches and reads, as far as its text tells: text searches (`rg`,
//! `grep`, `git grep`, …) and file reads (`cat`, `sed -n`, `head`, …) that are the first stage of
//! a pipeline. A later stage filters another command's output and is not reported.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A text search over files.
#[derive(Clone, Debug, PartialEq)]
pub struct Search {
    /// Where it searched: its first path argument, else the directory it ran in, when known.
    pub target: Option<PathBuf>,
    /// Whether it searched a directory tree rather than named files.
    pub tree: bool,
    /// Alternatives in its pattern (`a|b|c` is three).
    pub alternatives: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Command {
    pub searches: Vec<Search>,
    /// Files read; relative when the directory the command ran in is unknown.
    pub reads: Vec<PathBuf>,
}

/// Flags of the search programs that take the next word as their value.
const VALUE_FLAGS: &[&str] = &[
    "--include",
    "--exclude",
    "--exclude-dir",
    "--glob",
    "--type",
    "--type-not",
    "--max-count",
    "--max-depth",
    "--context",
    "--after-context",
    "--before-context",
    "--regexp",
    "--file",
];

/// Short flags of the search programs that take a value (`-e pattern`, `-A 3`, `-g glob`).
const VALUE_SHORT_FLAGS: &str = "efABCmgtTj";

/// Analyses `command`, run in `cwd`.
pub fn analyse(command: &str, cwd: Option<&Path>) -> Command {
    let mut out = Command::default();
    let mut dir = cwd.map(Path::to_path_buf);
    let mut vars: HashMap<String, String> = HashMap::new();
    for words in statements(command) {
        let words: Vec<String> = words.iter().map(|w| expand(w, &vars)).collect();
        let assignments = words.iter().take_while(|w| assignment(w).is_some()).count();
        let Some((program, args)) = words[assignments..].split_first() else {
            vars.extend(
                words
                    .iter()
                    .filter_map(|w| assignment(w))
                    .map(|(k, v)| (k.to_string(), v.to_string())),
            );
            continue;
        };
        match program.rsplit('/').next().unwrap_or(program) {
            "cd" => {
                if let Some(d) = args.first().and_then(|d| resolve(d, dir.as_deref())) {
                    dir = Some(d);
                }
            }
            name @ ("rg" | "grep" | "egrep" | "fgrep" | "ag" | "ack") => {
                out.searches
                    .push(search(args, !name.ends_with("grep"), dir.as_deref()));
            }
            "git" if args.first().is_some_and(|a| a == "grep") => {
                out.searches.push(search(&args[1..], true, dir.as_deref()));
            }
            "sed"
                if args.iter().any(|a| a == "-n") && !args.iter().any(|a| a.starts_with("-i")) =>
            {
                // The first operand is the script.
                let files = operands(args, "").skip(1);
                out.reads
                    .extend(files.filter_map(|p| resolve(p, dir.as_deref())));
            }
            name @ ("cat" | "head" | "tail" | "nl" | "bat" | "less") => {
                // `head -n 20 file`, but `cat -n file`.
                let short = if matches!(name, "head" | "tail") {
                    "nc"
                } else {
                    ""
                };
                let files =
                    operands(args, short).filter(|p| !p.bytes().all(|b| b.is_ascii_digit()));
                out.reads
                    .extend(files.filter_map(|p| resolve(p, dir.as_deref())));
            }
            _ => {}
        }
    }
    out
}

/// The search `args` describe; `tree` when the program searches directories by default.
fn search(args: &[String], tree: bool, dir: Option<&Path>) -> Search {
    let recursive = args.iter().any(|a| {
        a == "--recursive"
            || a.starts_with("--include")
            || (a.starts_with('-') && !a.starts_with("--") && a.contains(['r', 'R']))
    });
    let named = args
        .windows(2)
        .find(|w| w[0] == "--regexp" || (is_short(&w[0]) && w[0].ends_with('e')))
        .map(|w| w[1].as_str());
    let mut operands = operands(args, VALUE_SHORT_FLAGS);
    let pattern = named.or_else(|| operands.next()).unwrap_or_default();
    let target = operands
        .find_map(|p| {
            // A glob searches the directory it is in.
            let literal = p.split(['*', '?', '[', '{']).next().unwrap_or_default();
            let path = if literal.len() == p.len() {
                literal
            } else {
                literal.rsplit_once('/').map_or("", |(d, _)| d)
            };
            (!path.is_empty()).then(|| resolve(path, dir)).flatten()
        })
        .or_else(|| dir.map(Path::to_path_buf));
    Search {
        target,
        tree: tree || recursive,
        alternatives: pattern.matches('|').count() + 1,
    }
}

fn is_short(arg: &str) -> bool {
    arg.starts_with('-') && !arg.starts_with("--")
}

/// The words of `args` that are neither flags nor a flag's value; `short` lists the short flags
/// that take a value.
fn operands<'a>(args: &'a [String], short: &'a str) -> impl Iterator<Item = &'a str> {
    let mut skip = false;
    args.iter().map(String::as_str).filter(move |a| {
        if std::mem::take(&mut skip) {
            return false;
        }
        if !a.starts_with('-') || *a == "-" {
            return !a.contains(['<', '>']);
        }
        skip = VALUE_FLAGS.contains(a) || (is_short(a) && a.ends_with(|c| short.contains(c)));
        false
    })
}

/// `NAME=value`, split.
fn assignment(word: &str) -> Option<(&str, &str)> {
    let (name, value) = word.split_once('=')?;
    let valid = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some((name, value))
}

/// `word` with a leading `$NAME`, `${NAME}` or `~` replaced, where known.
fn expand(word: &str, vars: &HashMap<String, String>) -> String {
    if let Some(rest) = word.strip_prefix("~/") {
        return dirs::home_dir()
            .unwrap_or_default()
            .join(rest)
            .to_string_lossy()
            .into_owned();
    }
    let Some(rest) = word.strip_prefix('$') else {
        return word.to_string();
    };
    let (name, tail) = match rest.strip_prefix('{').and_then(|r| r.split_once('}')) {
        Some(braced) => braced,
        None => rest.split_at(
            rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len()),
        ),
    };
    match vars.get(name) {
        Some(value) => format!("{value}{tail}"),
        None => word.to_string(),
    }
}

/// `path` made absolute against `dir`; `None` when it holds something only the shell can expand.
fn resolve(path: &str, dir: Option<&Path>) -> Option<PathBuf> {
    if path.contains(['$', '`', '*', '?']) {
        return None;
    }
    let path = Path::new(path);
    Some(match dir {
        Some(dir) if path.is_relative() => dir.join(path),
        _ => path.to_path_buf(),
    })
}

/// The words of the first stage of each pipeline in `command`, quotes removed. Here-document
/// bodies are skipped.
fn statements(command: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    // Whether `word` exists even if empty (`''`), and whether a later pipeline stage is skipped.
    let (mut quoted, mut piped) = (false, false);
    // The line that ends the here-document a word of this line opens, then the one being skipped.
    let (mut opens, mut heredoc): (Option<String>, Option<String>) = (None, None);
    for line in command.lines() {
        if let Some(end) = &heredoc {
            if line.trim() == end {
                heredoc = None;
            }
            continue;
        }
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match (quote, c) {
                (Some(q), _) if c == q => quote = None,
                (Some('"'), '\\') if chars.peek().is_some_and(|n| "\"\\$`".contains(*n)) => {
                    word.extend(chars.next());
                }
                (Some(_), _) => word.push(c),
                (None, '\'' | '"') => (quote, quoted) = (Some(c), true),
                (None, '\\') => word.extend(chars.next()),
                (None, '>') if chars.peek() == Some(&'&') => {
                    word.push(c);
                    word.extend(chars.next());
                }
                (None, ' ' | '\t' | '(' | ')' | ';' | '&' | '|') => {
                    finish(&mut word, &mut quoted, piped, &mut words, &mut opens);
                    if matches!(c, ';' | '&') || (c == '|' && chars.peek() == Some(&'|')) {
                        chars.next_if(|n| *n == c);
                        out.push(std::mem::take(&mut words));
                        piped = false;
                    } else if c == '|' {
                        piped = true;
                    }
                }
                (None, _) => word.push(c),
            }
        }
        if quote.is_some() {
            // A quoted string continues on the next line.
            word.push('\n');
            continue;
        }
        finish(&mut word, &mut quoted, piped, &mut words, &mut opens);
        out.push(std::mem::take(&mut words));
        piped = false;
        heredoc = opens.take();
    }
    out.retain(|w| !w.is_empty());
    out
}

/// Ends `word`, if one was begun: it joins `words` unless a later pipeline stage, and a
/// here-document it opens is noted in `opens`.
fn finish(
    word: &mut String,
    quoted: &mut bool,
    piped: bool,
    words: &mut Vec<String>,
    opens: &mut Option<String>,
) {
    if !std::mem::take(quoted) && word.is_empty() {
        return;
    }
    let word = std::mem::take(word);
    if let Some((_, tag)) = word.split_once("<<") {
        *opens = Some(tag.trim_start_matches('-').to_string()).filter(|t| !t.is_empty());
    }
    if !piped {
        words.push(word);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(command: &str) -> Command {
        analyse(command, Some(Path::new("/w")))
    }

    fn targets(command: &str) -> Vec<Option<PathBuf>> {
        run(command)
            .searches
            .into_iter()
            .map(|s| s.target)
            .collect()
    }

    #[test]
    fn searches_over_files_not_filters() {
        assert_eq!(run("cd repo && rg -n foo src").searches.len(), 1);
        assert_eq!(run("LC_ALL=C grep -r foo .").searches.len(), 1);
        assert_eq!(run("git grep -n foo").searches.len(), 1);
        assert_eq!(run("test -d src || rg foo").searches.len(), 1);
        assert_eq!(run("grep -rn 'a;b' src | head").searches.len(), 1);
        for filter in [
            "cargo nextest run 2>&1 | grep -E 'FAIL|ok'",
            "git show HEAD:a.rs | grep -n foo",
            "git log --grep foo",
            "python3 - <<'EOF'\ngrep = 1\nhead = lines[0]\nEOF",
        ] {
            assert_eq!(run(filter), Command::default(), "{filter}");
        }
    }

    #[test]
    fn search_shape() {
        let s = &run(r#"grep -rn 'def observe\|def snapshot\|"labels"' service"#).searches[0];
        assert_eq!((s.tree, s.alternatives), (true, 3));
        let s = &run("grep -n 'def \\|class ' service/a.py service/b.py").searches[0];
        assert_eq!((s.tree, s.alternatives), (false, 2));
        let s = &run("rg -t py -e 'a|b|c|d'").searches[0];
        assert_eq!((s.tree, s.alternatives), (true, 4));
        assert!(run("grep --include='*.py' foo .").searches[0].tree);
    }

    #[test]
    fn search_targets() {
        let path = |p: &str| Some(PathBuf::from(p));
        assert_eq!(targets("rg foo"), [path("/w")]);
        assert_eq!(targets("cd /r/x && grep -rn foo src"), [path("/r/x/src")]);
        assert_eq!(targets("grep -n foo src/*.py"), [path("/w/src")]);
        assert_eq!(
            targets("D=/tmp/s; grep -c foo $D/a.json"),
            [path("/tmp/s/a.json")]
        );
        assert_eq!(targets("grep foo ${UNKNOWN}/a"), [path("/w")]);
        assert_eq!(analyse("rg foo", None).searches[0].target, None);
    }

    #[test]
    fn reads() {
        let paths = |command: &str| run(command).reads;
        let path = PathBuf::from;
        assert_eq!(
            paths("cat a/b.py; sed -n 1,80p c.py; sed -n '5,9p;20p' /abs/d.rs"),
            [path("/w/a/b.py"), path("/w/c.py"), path("/abs/d.rs")]
        );
        assert_eq!(
            paths("cd sub && head -n 20 x.rs | nl"),
            [path("/w/sub/x.rs")]
        );
        assert_eq!(paths("tail -5 y.log 2>/dev/null"), [path("/w/y.log")]);
        assert_eq!(
            paths("cargo test | head -20; sed -i s/a/b/ z; cat $F"),
            [] as [PathBuf; 0]
        );
        assert_eq!(analyse("cat a.rs", None).reads, [path("a.rs")]);
    }
}
