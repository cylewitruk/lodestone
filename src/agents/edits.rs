//! The `PreToolUse` hint for Claude Code's file edits (`Edit`, `MultiEdit`, `Write`): the existing
//! definitions most like those an edit adds, found by the daemon's search.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::api::types::{SearchRequest, SearchResult};
use crate::chunk::{Chunker, Item, LanguageRegistry};
use crate::client::Client;
use crate::config::Config;

/// How [`hint`] text starts; transcript statistics recognise it by this.
pub const EDIT_HINT_OPENING: &str = "Existing code resembles what this edit adds: ";

/// Matches the hint names at most.
const MAX_MATCHES: usize = 3;

/// Results per definition searched, leaving room for the edited definitions themselves.
const TOP_K: usize = 5;

/// One replacement in the edited file.
#[derive(Debug, PartialEq)]
struct Edit {
    /// The text replaced; `None` for the whole file.
    old: Option<String>,
    new: String,
}

/// The file a tool call writes and the replacements it makes, if it is a file edit.
fn edits(tool: &str, input: &Value) -> Option<(PathBuf, Vec<Edit>)> {
    let path = PathBuf::from(input["file_path"].as_str()?);
    let replace = |e: &Value| {
        Some(Edit {
            old: Some(e["old_string"].as_str()?.to_string()),
            new: e["new_string"].as_str()?.to_string(),
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
        }],
        _ => return None,
    };
    Some((path, edits))
}

/// Lines `item` spans in the text it was chunked from.
fn source_of(text: &str, item: &Item) -> String {
    let skip = item.line_start.saturating_sub(1) as usize;
    let take = (item.line_end + 1).saturating_sub(item.line_start) as usize;
    text.lines()
        .skip(skip)
        .take(take)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The definitions `edit` writes that its replaced text (`current`, the file, for a whole-file
/// write) does not already hold verbatim. Tests and definitions holding others are left out;
/// those others are each checked.
fn added(chunker: &Chunker, path: &str, edit: &Edit, current: &str) -> Vec<Item> {
    let old = edit.old.as_deref().unwrap_or(current);
    let items = chunker.chunk(path, edit.new.as_bytes());
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
        .filter(|it| !old.contains(source_of(&edit.new, it).trim()))
        .cloned()
        .collect()
}

/// The first and last lines (1-based) of each part of `current` that `edits` replace.
fn replaced(edits: &[Edit], current: &str) -> Vec<(u32, u32)> {
    let line_of = |at: usize| current[..at].matches('\n').count() as u32 + 1;
    edits
        .iter()
        .flat_map(|e| match e.old.as_deref() {
            None => vec![(1, u32::MAX)],
            Some("") => Vec::new(),
            Some(old) => current
                .match_indices(old)
                .map(|(at, _)| (line_of(at), line_of(at + old.len() - 1)))
                .collect(),
        })
        .collect()
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
/// [`crate::config::HooksConfig::duplicate_similarity`] like a definition the edit adds, other than
/// the ones it replaces. `None` for any other tool, without a daemon, or on any error.
pub fn hint(tool: &str, input: &Value, dir: &Path, config: &Config) -> Option<String> {
    let (path, edits) = edits(tool, input)?;
    let path = canonical(&dir.join(path))?;
    let name = path.to_string_lossy();
    let current = fs::read_to_string(&path).unwrap_or_default();
    let languages = LanguageRegistry::load_for(&config.languages, &config.data_dir(), &path);
    let chunker = Chunker::new(Arc::new(languages), config.chunk_tokens);
    let added: Vec<Item> = edits
        .iter()
        .flat_map(|e| added(&chunker, &name, e, &current))
        .collect();
    if added.is_empty() {
        return None;
    }
    let replaced = replaced(&edits, &current);
    // The daemon resolves the worktree from a directory that exists.
    let repo = path.ancestors().skip(1).find(|p| p.is_dir())?;
    let client = Client::connect(&config.endpoint())?;
    let threshold = config.hooks.duplicate_similarity;
    let mut matches: Vec<(f32, PathBuf, SearchResult, &str)> = Vec::new();
    for item in &added {
        // The chunk's text after its header line: what the index embeds, without the location.
        let query = item.text.split_once('\n').map_or("", |(_, code)| code);
        let resp = client
            .search(&SearchRequest {
                query: query.to_string(),
                repo: Some(repo.to_string_lossy().into_owned()),
                language: Some(item.language.clone()),
                top_k: TOP_K,
                ..SearchRequest::default()
            })
            .ok()?;
        let root = Path::new(&resp.repo);
        let rel = path.strip_prefix(root).ok()?;
        let edited = |r: &SearchResult| {
            Path::new(&r.path) == rel
                && (r.name == item.name
                    || replaced
                        .iter()
                        .any(|&(first, last)| first <= r.line_end && r.line <= last))
        };
        let best = resp
            .results
            .into_iter()
            .filter(|r| !edited(r))
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
            let file = file.strip_prefix(&dir).unwrap_or(file);
            format!("{}:{} {} (like `{new}`)", file.display(), r.line, r.name)
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

    fn chunker() -> Chunker {
        let config = Config::default();
        let languages = LanguageRegistry::load_for(
            &config.languages,
            Path::new("/nonexistent"),
            "a.rs".as_ref(),
        );
        Chunker::new(Arc::new(languages), config.chunk_tokens)
    }

    fn edit(old: Option<&str>, new: &str) -> Edit {
        Edit {
            old: old.map(String::from),
            new: new.into(),
        }
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
                    { "old_string": "c", "new_string": "d" },
                ] })
            ),
            Some((
                path.clone(),
                vec![edit(Some("a"), "b"), edit(Some("c"), "d")]
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
    fn only_new_or_changed_leaf_definitions_are_checked() {
        let kept = "fn kept() -> u32 {\n    1\n}\n";
        let new = format!(
            "{kept}\nfn changed() -> u32 {{\n    3\n}}\n\nstruct Holder;\n\nimpl Holder {{\n    fn method(&self) {{}}\n}}\n\n#[test]\nfn a_test() {{}}\n"
        );
        let old = format!("{kept}\nfn changed() -> u32 {{\n    2\n}}\n");
        let names = |e: &Edit, current: &str| -> Vec<String> {
            added(&chunker(), "src/a.rs", e, current)
                .into_iter()
                .map(|i| i.qualified_name)
                .collect()
        };
        let expected = ["changed", "Holder", "Holder::method"];
        assert_eq!(names(&edit(Some(&old), &new), ""), expected);
        // A whole-file write compares with the file as it is.
        assert_eq!(names(&edit(None, &new), &old), expected);
        // Text without definitions, or in no known language, adds none.
        assert!(names(&edit(Some("1"), "    2"), "").is_empty());
        assert!(added(&chunker(), "a.unknown", &edit(None, &new), "").is_empty());
    }

    #[test]
    fn replaced_lines_are_where_the_old_text_is() {
        let current = "a\nb\nc\nb\n";
        assert_eq!(replaced(&[edit(Some("b\nc\n"), "x")], current), [(2, 3)]);
        assert_eq!(replaced(&[edit(Some("b"), "x")], current), [(2, 2), (4, 4)]);
        assert_eq!(replaced(&[edit(None, "x")], current), [(1, u32::MAX)]);
        assert!(replaced(&[edit(Some(""), "x")], current).is_empty());
    }
}
