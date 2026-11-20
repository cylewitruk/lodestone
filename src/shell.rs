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
    /// Whether it searched a git revision rather than the files on disk.
    pub revision: bool,
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
                let args = &args[1..];
                out.searches.push(Search {
                    revision: git_grep_revision(args, dir.as_deref()),
                    ..search(args, true, dir.as_deref())
                });
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

/// The search `args` describe; `tree` when the program searches directories by default. It does
/// not search a tree when every path it names is an existing file.
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
    let paths: Vec<Option<PathBuf>> = operands
        .map(|p| {
            // A glob searches the directory it is in.
            let literal = p.split(['*', '?', '[', '{']).next().unwrap_or_default();
            let path = if literal.len() == p.len() {
                literal
            } else {
                literal.rsplit_once('/').map_or("", |(d, _)| d)
            };
            (!path.is_empty()).then(|| resolve(path, dir)).flatten()
        })
        .collect();
    let named_files = !paths.is_empty()
        && paths
            .iter()
            .all(|p| p.as_ref().is_some_and(|p| p.is_file()));
    Search {
        target: paths
            .into_iter()
            .flatten()
            .next()
            .or_else(|| dir.map(Path::to_path_buf)),
        tree: (tree || recursive) && !named_files,
        alternatives: pattern.matches('|').count() + 1,
        revision: false,
    }
}

/// Whether `git grep` `args` name a revision to search (`git grep foo main -- src`). Before `--`,
/// every operand after the pattern is one; without `--`, git takes an operand that is no path in
/// `dir` as one. Pathspec globs and magic (`'*.rs'`, `:!vendor`) are paths.
fn git_grep_revision(args: &[String], dir: Option<&Path>) -> bool {
    let separator = args.iter().position(|a| a == "--");
    let before = &args[..separator.unwrap_or(args.len())];
    let named = before
        .iter()
        .any(|a| a.starts_with("--regexp") || (is_short(a) && a.ends_with('e')));
    let mut trees = operands(before, VALUE_SHORT_FLAGS).skip(usize::from(!named));
    match (separator, dir) {
        (Some(_), _) => trees.next().is_some(),
        (None, Some(_)) => trees.any(|t| {
            !t.starts_with(':')
                && !t.contains(['*', '?', '['])
                && resolve(t, dir).is_some_and(|p| !p.exists())
        }),
        (None, None) => false,
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

/// The words of the first stage of each pipeline in `command`, quotes removed. Operators (`;`,
/// `&`, `|`, `|&`, `&&`, `||`, a newline, `<<`) and comments count only outside quotes;
/// here-document bodies are skipped, and a line ending in `|`, `&&`, `||` or `\` continues on the
/// next. `eval/situations/score.py` mirrors this.
fn statements(command: &str) -> Vec<Vec<String>> {
    let mut t = Tokens::default();
    let mut rest = command;
    while let Some(c) = rest.chars().next() {
        let after = &rest[c.len_utf8()..];
        if let Some(q) = t.quote {
            let word = t.word.get_or_insert_default();
            if c == q {
                t.quote = None;
            } else if q == '"' && c == '\\' && after.starts_with(['"', '\\', '$', '`']) {
                word.push_str(&after[..1]);
                rest = &after[1..];
                continue;
            } else {
                word.push(c);
            }
            rest = after;
            continue;
        }
        let (op, len) = match c {
            '\'' | '"' => {
                t.quote = Some(c);
                t.word.get_or_insert_default();
                (None, 1)
            }
            '\\' if after.starts_with('\n') => (None, 2),
            '\\' => {
                let next = after.chars().next();
                t.word.get_or_insert_default().extend(next);
                (None, 1 + next.map_or(0, char::len_utf8))
            }
            '#' if t.word.is_none() => (None, rest.find('\n').unwrap_or(rest.len())),
            ' ' | '\t' | '(' | ')' => {
                t.end_word();
                (None, 1)
            }
            '\n' => {
                t.end_word();
                rest = t.skip_heredocs(after);
                if !t.continued {
                    t.end_statement("\n");
                }
                continue;
            }
            _ if rest.starts_with("&&") || rest.starts_with("||") || rest.starts_with("|&") => {
                (Some(&rest[..2]), 2)
            }
            _ if rest.starts_with("<<<") => {
                t.end_word();
                (None, 3)
            }
            _ if rest.starts_with("<<") => {
                t.end_word();
                let tabs = rest.starts_with("<<-");
                t.heredoc = Some(tabs);
                (None, if tabs { 3 } else { 2 })
            }
            // `2>&1`
            '&' if t.word.as_deref().is_some_and(|w| w.ends_with(['>', '<'])) => {
                t.word.get_or_insert_default().push(c);
                (None, 1)
            }
            ';' | '&' | '|' => (Some(&rest[..1]), 1),
            _ => {
                t.word.get_or_insert_default().push(c);
                (None, c.len_utf8())
            }
        };
        if let Some(op) = op {
            t.end_statement(op);
        }
        rest = &rest[len..];
    }
    if t.quote.is_none() {
        t.end_statement("");
    }
    t.out
}

#[derive(Default)]
struct Tokens {
    out: Vec<Vec<String>>,
    words: Vec<String>,
    /// The word being read; `Some("")` after `''`.
    word: Option<String>,
    quote: Option<char>,
    /// The statement reads a pipe, so it is not reported.
    piped: bool,
    /// The last operator needs a command after it, so a newline continues the statement.
    continued: bool,
    /// The next word names a here-document; `true` for `<<-`, which strips leading tabs.
    heredoc: Option<bool>,
    /// Here-documents whose bodies follow the current line.
    delimiters: Vec<(String, bool)>,
}

impl Tokens {
    fn end_word(&mut self) {
        let Some(word) = self.word.take() else {
            return;
        };
        match self.heredoc.take() {
            Some(tabs) => self.delimiters.push((word, tabs)),
            None => {
                self.words.push(word);
                self.continued = false;
            }
        }
    }

    fn end_statement(&mut self, op: &str) {
        self.end_word();
        let words = std::mem::take(&mut self.words);
        if !words.is_empty() && !self.piped {
            self.out.push(words);
        }
        self.piped = matches!(op, "|" | "|&");
        self.continued = matches!(op, "|" | "|&" | "&&" | "||");
    }

    /// `rest` past the bodies of the here-documents the line just ended opened.
    fn skip_heredocs<'a>(&mut self, mut rest: &'a str) -> &'a str {
        while !self.delimiters.is_empty() && !rest.is_empty() {
            let (line, next) = rest.split_once('\n').unwrap_or((rest, ""));
            let (delimiter, tabs) = &self.delimiters[0];
            if (if *tabs {
                line.trim_start_matches('\t')
            } else {
                line
            }) == delimiter
            {
                self.delimiters.remove(0);
            }
            rest = next;
        }
        rest
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

    /// The cases `eval/situations/test_score.py` checks too.
    #[test]
    fn shared_cases() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/shell-searches.json")).unwrap();
        for (key, searches) in [("search", true), ("no_search", false)] {
            for c in cases[key].as_array().unwrap() {
                let c = c.as_str().unwrap();
                assert_eq!(!run(c).searches.is_empty(), searches, "{c:?}");
            }
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
    fn named_files_are_not_a_tree() {
        let dir = crate::testutil::tempdir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        let tree = |c: &str| analyse(c, Some(&dir)).searches[0].tree;
        assert!(!tree("rg 'a|b|c' src/lib.rs"));
        assert!(!tree("grep -rn 'a|b|c' src/lib.rs"));
        assert!(tree("rg 'a|b|c' src/lib.rs src"));
        assert!(tree("rg 'a|b|c' src/missing.rs"));
        assert!(tree("rg 'a|b|c'"));
    }

    #[test]
    fn git_grep_over_a_revision() {
        let dir = crate::testutil::tempdir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let revision = |c: &str| analyse(c, Some(&dir)).searches[0].revision;
        assert!(revision("git grep -n -E 'a|b' upstream/main -- src"));
        assert!(revision("M=upstream/main; git grep -n foo $M -- src"));
        assert!(revision("git grep foo HEAD~2"));
        assert!(revision("git grep -e foo -e bar v1.0"));
        assert!(!revision("git grep -n foo -- src missing"));
        assert!(!revision("git grep -n foo src '*.rs' ':!vendor'"));
        assert!(!revision("git grep --cached foo"));
        assert!(!revision("grep -rn foo missing"));
        assert!(!analyse("git grep foo HEAD", None).searches[0].revision);
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
