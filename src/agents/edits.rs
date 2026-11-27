//! The `PreToolUse` hint for Claude Code's file edits (`Edit`, `MultiEdit`, `Write`): the existing
//! definitions most like those an edit adds, found by the daemon's search.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::api::types::{SearchRequest, SearchResult};
use crate::chunk::{Chunker, Item, LanguageRegistry, LineIndex};
use crate::client::Client;
use crate::config::Config;

/// How [`hint`] text starts; transcript statistics recognise it by this.
pub const EDIT_HINT_OPENING: &str = "Existing code resembles what this edit adds: ";

/// Matches the hint names at most.
const MAX_MATCHES: usize = 3;

/// Results per definition searched, leaving room for the edited definitions themselves.
const TOP_K: usize = 5;

/// How long the hint's searches may take in all; it names what they found by then.
const SEARCH_BUDGET: Duration = Duration::from_secs(2);

/// One replacement in the edited file.
#[derive(Debug, PartialEq)]
struct Edit {
    /// The text replaced; `None` for the whole file.
    old: Option<String>,
    new: String,
    /// Every occurrence of `old` (`replace_all`), not only the first.
    all: bool,
}

/// The file a tool call writes and the replacements it makes, if it is a file edit.
fn edits(tool: &str, input: &Value) -> Option<(PathBuf, Vec<Edit>)> {
    let path = PathBuf::from(input["file_path"].as_str()?);
    let replace = |e: &Value| {
        Some(Edit {
            old: Some(e["old_string"].as_str()?.to_string()),
            new: e["new_string"].as_str()?.to_string(),
            all: e["replace_all"].as_bool().unwrap_or(false),
        })
    };
    let edits = match tool {
        "Edit" => vec![replace(input)?],
        "MultiEdit" => input["edits"]
            .as_array()?
            .iter()
            .filter_map(replace)
            .collect(),
        "Write" => vec![Edit {
            old: None,
            new: input["content"].as_str()?.to_string(),
            all: false,
        }],
        _ => return None,
    };
    Some((path, edits))
}

/// A file after a tool call's edits.
#[derive(Debug, PartialEq)]
struct Edited {
    text: String,
    /// Lines (1-based) of `text` holding non-whitespace the edits wrote.
    written: BTreeSet<u32>,
    /// Lines (1-based) of the file as it was that the edits change: lines holding non-whitespace
    /// they replace, and lines they insert text within.
    replaced: BTreeSet<u32>,
}

/// Bytes `a` and `b` share at their start and, of the rest, at their end, cut at characters.
fn common_ends(a: &str, b: &str) -> (usize, usize) {
    let boundary = |at: usize, back: bool| {
        let (ia, ib) = if back {
            (a.len() - at, b.len() - at)
        } else {
            (at, at)
        };
        a.is_char_boundary(ia) && b.is_char_boundary(ib)
    };
    let mut front = iter::zip(a.bytes(), b.bytes())
        .take_while(|(x, y)| x == y)
        .count();
    while !boundary(front, false) {
        front -= 1;
    }
    let mut back = iter::zip(a.bytes().rev(), b.bytes().rev())
        .take(a.len().min(b.len()) - front)
        .take_while(|(x, y)| x == y)
        .count();
    while !boundary(back, true) {
        back -= 1;
    }
    (front, back)
}

/// `current` with `edits` applied in order, as the tool applies them; `None` when an edit's text
/// is not in the file, so the call fails. Of each replacement only what differs from the text it
/// replaces counts, not the context repeated around it.
fn apply(current: &str, edits: &[Edit]) -> Option<Edited> {
    let mut text = current.to_string();
    // Per byte of `text`: its offset in `current`, `None` where an edit wrote it.
    let mut origin: Vec<Option<usize>> = (0..current.len()).map(Some).collect();
    // Offsets in `current` on the lines the edits change.
    let mut changed: Vec<usize> = Vec::new();
    for e in edits {
        let (starts, len): (Vec<usize>, usize) = match e.old.as_deref() {
            None => (vec![0], text.len()),
            Some("") if text.is_empty() => (vec![0], 0),
            Some("") => return None,
            Some(old) => {
                let found = text.match_indices(old).map(|(at, _)| at);
                let n = if e.all { usize::MAX } else { 1 };
                (found.take(n).collect(), old.len())
            }
        };
        if starts.is_empty() {
            return None;
        }
        // From the last, so the earlier offsets stay valid.
        for start in starts.into_iter().rev() {
            let old = &text.as_bytes()[start..start + len];
            let (mut front, mut back) = common_ends(&text[start..start + len], &e.new);
            // A pure insertion that can start a line instead (`fn c` → `fn b() {}\nfn c`) does,
            // so it inserts whole lines.
            if front + back == len {
                let line_start = (0..=front).find(|&k| {
                    let at = start + front - k;
                    at == 0 || text.as_bytes()[at - 1] == b'\n'
                });
                if let Some(k) = line_start
                    && e.new.len() >= back + k
                    && e.new.as_bytes()[e.new.len() - back - k..] == old[front - k..]
                {
                    front -= k;
                    back += k;
                }
            }
            let inner = start + front..start + len - back;
            let new = &e.new[front..e.new.len() - back];
            changed.extend(
                origin[inner.clone()]
                    .iter()
                    .flatten()
                    .filter(|&&at| !current.as_bytes()[at].is_ascii_whitespace()),
            );
            // Text inserted within a line changes it; whole lines inserted between others don't.
            if inner.is_empty() && !new.is_empty() {
                let bytes = text.as_bytes();
                let at_end = bytes.get(inner.start).is_none_or(|&b| b == b'\n');
                let at_start = inner.start == 0 || bytes[inner.start - 1] == b'\n';
                let whole_lines =
                    (at_end && new.starts_with('\n')) || (at_start && new.ends_with('\n'));
                let near = [inner.start.checked_sub(1), Some(inner.start)];
                let line = near.into_iter().flatten().find_map(|at| *origin.get(at)?);
                changed.extend(line.filter(|_| !whole_lines));
            }
            text.replace_range(inner.clone(), new);
            origin.splice(inner, iter::repeat_n(None, new.len()));
        }
    }
    let written = (0..text.len())
        .filter(|&at| origin[at].is_none() && !text.as_bytes()[at].is_ascii_whitespace());
    Some(Edited {
        written: line_set(&text, written),
        replaced: line_set(current, changed.into_iter()),
        text,
    })
}

/// The lines (1-based) of `source` holding byte offsets `offsets`.
fn line_set(source: &str, offsets: impl Iterator<Item = usize>) -> BTreeSet<u32> {
    let index = LineIndex::new(source.as_bytes());
    offsets.map(|at| index.line_of(at)).collect()
}

/// Whether `lines` holds any of `first..=last`.
fn touches(lines: &BTreeSet<u32>, first: u32, last: u32) -> bool {
    lines.range(first..=last.max(first)).next().is_some()
}

/// The definitions of the edited file at `path` that the edits wrote into. Tests and definitions
/// holding others are left out; those others are each checked.
fn added(chunker: &Chunker, path: &str, edited: &Edited) -> Vec<Item> {
    let items = chunker.chunk(path, edited.text.as_bytes());
    let holds_others = |it: &Item| {
        let prefix = format!("{}::", it.qualified_name);
        items.iter().any(|o| {
            o.qualified_name.starts_with(&prefix)
                && it.line_start <= o.line_start
                && o.line_end <= it.line_end
        })
    };
    items
        .iter()
        .filter(|it| it.tagged && !it.is_test && !holds_others(it))
        .filter(|it| touches(&edited.written, it.line_start, it.line_end))
        .cloned()
        .collect()
}

/// Whether search result `r` is the definition `item` changes rather than another: in the edited
/// file `rel`, in the same scope under the same name, or on lines the edits replace.
fn is_edited(r: &SearchResult, rel: &Path, item: &Item, edited: &Edited) -> bool {
    Path::new(&r.path) == rel
        && (r.qualified_name == item.qualified_name
            || touches(&edited.replaced, r.line, r.line_end))
}

/// `path` with symlinks resolved, also when the file does not exist yet.
fn canonical(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok().or_else(|| {
        let parent = canonical(path.parent()?)?;
        Some(parent.join(path.file_name()?))
    })
}

/// The `PreToolUse` hook's context for file-editing tool `tool` called with `input` in `dir`: the
/// existing definitions (up to [`MAX_MATCHES`]) at least
/// [`crate::config::HooksConfig::duplicate_similarity`] like a definition the edit writes, other
/// than the ones it changes. Searches stop after [`SEARCH_BUDGET`] or an error, keeping what they
/// found. `None` for any other tool, without a daemon, or when nothing is found.
pub fn hint(tool: &str, input: &Value, dir: &Path, config: &Config) -> Option<String> {
    let (path, edits) = edits(tool, input)?;
    let path = canonical(&dir.join(path))?;
    let current = fs::read_to_string(&path).unwrap_or_default();
    let edited = apply(&current, &edits)?;
    let languages = LanguageRegistry::load_for(&config.languages, &config.data_dir(), &path);
    let chunker = Chunker::new(Arc::new(languages), config.chunk_tokens);
    let added = added(&chunker, &path.to_string_lossy(), &edited);
    if added.is_empty() {
        return None;
    }
    let deadline = Instant::now() + SEARCH_BUDGET;
    // The daemon resolves the worktree from a directory that exists.
    let repo = path.ancestors().skip(1).find(|p| p.is_dir())?;
    let client = Client::connect(&config.endpoint())?;
    let threshold = config.hooks.duplicate_similarity;
    let mut matches: Vec<(f32, PathBuf, SearchResult, &str)> = Vec::new();
    for item in &added {
        if Instant::now() >= deadline {
            break;
        }
        // The chunk's text after its header line: what the index embeds, without the location.
        let query = item.text.split_once('\n').map_or("", |(_, code)| code);
        let req = SearchRequest {
            query: query.to_string(),
            repo: Some(repo.to_string_lossy().into_owned()),
            language: Some(item.language.clone()),
            top_k: TOP_K,
            ..SearchRequest::default()
        };
        let Ok(resp) = client.search_before(&req, deadline) else {
            break;
        };
        let root = Path::new(&resp.repo);
        let Ok(rel) = path.strip_prefix(root) else {
            break;
        };
        let best = resp
            .results
            .into_iter()
            .filter(|r| !is_edited(r, rel, item, &edited))
            .filter_map(|r| Some((r.similarity?, r)))
            .filter(|(s, _)| *s >= threshold)
            .max_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((similarity, r)) = best {
            matches.push((similarity, root.join(&r.path), r, &item.name));
        }
    }
    matches.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut seen = HashSet::new();
    matches.retain(|(_, file, r, _)| seen.insert((file.clone(), r.line)));
    if matches.is_empty() {
        return None;
    }
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let names: Vec<String> = matches
        .iter()
        .take(MAX_MATCHES)
        .map(|(_, file, r, new)| {
            // `/`-separated like search results: joining onto a Windows verbatim root makes `\`.
            let file = file.strip_prefix(&dir).unwrap_or(file).to_string_lossy();
            let file = file.replace('\\', "/");
            format!("{file}:{} {} (like `{new}`)", r.line, r.name)
        })
        .collect();
    Some(format!(
        "{EDIT_HINT_OPENING}{}. Reuse what fits instead of writing a new version.",
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunker(path: &str) -> Chunker {
        let config = Config::default();
        let languages =
            LanguageRegistry::load_for(&config.languages, Path::new("/nonexistent"), path.as_ref());
        Chunker::new(Arc::new(languages), config.chunk_tokens)
    }

    fn edit(old: Option<&str>, new: &str) -> Edit {
        Edit {
            old: old.map(String::from),
            new: new.into(),
            all: false,
        }
    }

    /// The qualified names of the definitions `edits` write into `current`, a file at `path`.
    fn added_names(path: &str, current: &str, edits: &[Edit]) -> Vec<String> {
        let edited = apply(current, edits).unwrap();
        added(&chunker(path), path, &edited)
            .into_iter()
            .map(|i| i.qualified_name)
            .collect()
    }

    #[test]
    fn reads_each_file_editing_tool() {
        let path = PathBuf::from("/r/a.rs");
        assert_eq!(
            edits(
                "Edit",
                &json!({ "file_path": "/r/a.rs", "old_string": "a", "new_string": "b" })
            ),
            Some((path.clone(), vec![edit(Some("a"), "b")]))
        );
        assert_eq!(
            edits(
                "MultiEdit",
                &json!({ "file_path": "/r/a.rs", "edits": [
                    { "old_string": "a", "new_string": "b" },
                    { "old_string": "c", "new_string": "d", "replace_all": true },
                ] })
            ),
            Some((
                path.clone(),
                vec![
                    edit(Some("a"), "b"),
                    Edit {
                        all: true,
                        ..edit(Some("c"), "d")
                    }
                ]
            ))
        );
        assert_eq!(
            edits("Write", &json!({ "file_path": "/r/a.rs", "content": "x" })),
            Some((path, vec![edit(None, "x")]))
        );
        assert_eq!(edits("Read", &json!({ "file_path": "/r/a.rs" })), None);
        assert_eq!(edits("Edit", &json!({ "file_path": "/r/a.rs" })), None);
    }

    #[test]
    fn edits_apply_in_order_and_mark_only_what_changed() {
        let current = "a\nb\nc\nb\n";
        let applied = apply(current, &[edit(Some("b\nc"), "b\nx")]).unwrap();
        assert_eq!(applied.text, "a\nb\nx\nb\n");
        // The repeated `b` is context, not a change.
        assert_eq!(applied.written, BTreeSet::from([3]));
        assert_eq!(applied.replaced, BTreeSet::from([3]));
        let all = Edit {
            all: true,
            ..edit(Some("b"), "y")
        };
        let applied = apply(current, &[all, edit(Some("a\n"), "")]).unwrap();
        assert_eq!(applied.text, "y\nc\ny\n");
        assert_eq!(applied.written, BTreeSet::from([1, 3]));
        assert_eq!(applied.replaced, BTreeSet::from([1, 2, 4]));
        // Lines inserted between others change none of them; text inserted within a line does.
        let current = "fn a() {}\nfn c() {}\n";
        let after_a = [edit(Some("fn a() {}"), "fn a() {}\nfn b() {}")];
        let before_c = [edit(Some("fn c"), "fn b() {}\nfn c")];
        for edits in [&after_a, &before_c] {
            let applied = apply(current, edits).unwrap();
            assert_eq!(applied.text, "fn a() {}\nfn b() {}\nfn c() {}\n");
            assert_eq!(applied.written, BTreeSet::from([2]));
            assert!(applied.replaced.is_empty(), "{edits:?}");
        }
        let renamed = apply(current, &[edit(Some("fn c"), "fn c2")]).unwrap();
        assert_eq!(renamed.replaced, BTreeSet::from([2]));
        // A whole-file write of a new file writes all of it.
        let applied = apply("", &[edit(None, "a\n\nb")]).unwrap();
        assert_eq!(applied.written, BTreeSet::from([1, 3]));
        // An edit whose text is not in the file fails, as the tool call does.
        assert_eq!(apply(current, &[edit(Some("z"), "y")]), None);
        assert_eq!(apply(current, &[edit(Some(""), "y")]), None);
    }

    #[test]
    fn multibyte_text_around_a_change_is_counted_whole() {
        let current = "fn foo() {}\n// é\n";
        let applied = apply(
            current,
            &[edit(Some("fn foo() {}\n// é"), "fn bar() {}\n// é")],
        );
        let applied = applied.unwrap();
        assert_eq!(applied.text, "fn bar() {}\n// é\n");
        assert_eq!(applied.written, BTreeSet::from([1]));
        let applied = apply("é", &[edit(Some("é"), "è")]).unwrap();
        assert_eq!((applied.text.as_str(), applied.written.len()), ("è", 1));
    }

    #[test]
    fn only_written_leaf_definitions_are_checked() {
        let kept = "fn kept() -> u32 {\n    1\n}\n";
        let old = format!("{kept}\nfn changed() -> u32 {{\n    2\n}}\n");
        let new = format!(
            "{kept}\nfn changed() -> u32 {{\n    3\n}}\n\nstruct Holder;\n\nimpl Holder {{\n    fn method(&self) {{}}\n}}\n\n#[test]\nfn a_test() {{}}\n"
        );
        let expected = ["changed", "Holder", "Holder::method"];
        assert_eq!(
            added_names("src/a.rs", &old, &[edit(Some(&old), &new)]),
            expected
        );
        assert_eq!(added_names("src/a.rs", &old, &[edit(None, &new)]), expected);
        // An edit inside a definition writes into it; one with none adds nothing.
        let body = [edit(Some("    1"), "    10")];
        assert_eq!(added_names("src/a.rs", &old, &body), ["kept"]);
        assert!(added_names("src/a.rs", &old, &[edit(Some("\n\n"), "\n\n\n")]).is_empty());
        assert!(added_names("a.unknown", "", &[edit(None, &new)]).is_empty());
    }

    #[test]
    fn edits_are_parsed_within_their_file() {
        // Neither fragment parses on its own: it lacks its `<script>` block or `<?php` tag.
        let svelte = "<script lang=\"ts\">\nfunction a() {}\n</script>\n<p>hi</p>\n";
        let add = [edit(
            Some("function a() {}"),
            "function a() {}\nfunction b() {}",
        )];
        assert_eq!(added_names("App.svelte", svelte, &add), ["b"]);
        let php = "<?php\nfunction a() {}\n";
        assert_eq!(added_names("a.php", php, &add), ["b"]);
    }

    #[test]
    fn a_copy_under_another_scope_is_not_the_edited_definition() {
        let current = "struct A;\n\nimpl A {\n    fn parse(&self) -> u32 {\n        7\n    }\n}\n";
        let copy = "\nstruct B;\n\nimpl B {\n    fn parse(&self) -> u32 {\n        7\n    }\n}\n";
        let edited = apply(current, &[edit(Some(current), &format!("{current}{copy}"))]).unwrap();
        let added = added(&chunker("src/a.rs"), "src/a.rs", &edited);
        let names: Vec<&str> = added.iter().map(|i| i.qualified_name.as_str()).collect();
        assert_eq!(names, ["B", "B::parse"]);
        let result = |path: &str, qualified_name: &str, line: u32| SearchResult {
            path: path.into(),
            line,
            line_end: line + 2,
            qualified_name: qualified_name.into(),
            ..SearchResult::default()
        };
        let rel = Path::new("src/a.rs");
        let parse = &added[1];
        // The unchanged `A::parse` is a match, not the edited definition.
        assert!(!is_edited(
            &result("src/a.rs", "A::parse", 4),
            rel,
            parse,
            &edited
        ));
        assert!(is_edited(
            &result("src/a.rs", "B::parse", 12),
            rel,
            parse,
            &edited
        ));
        assert!(!is_edited(
            &result("src/b.rs", "B::parse", 12),
            rel,
            parse,
            &edited
        ));
        // A definition the edit replaces, renamed or not, is the edited one.
        let renamed = apply(current, &[edit(Some("parse"), "parse_all")]).unwrap();
        assert!(is_edited(
            &result("src/a.rs", "A::parse", 4),
            rel,
            parse,
            &renamed
        ));
    }
}
