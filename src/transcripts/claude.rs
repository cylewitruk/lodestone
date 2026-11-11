//! Claude Code transcripts: `<projects>/<project>/<session>.jsonl`, plus subagents in
//! `<session>/subagents/*.jsonl` under their parent's session id. A project directory is named
//! after its working directory; each line carries its session, working directory and timestamp.
//! Tool calls are `tool_use` blocks and their results `tool_result` blocks.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Call, Event, Kind, jsonl_files, query_of, text_of, time};

/// The transcripts of projects whose working directory may be in or under one of `roots`. Other
/// projects are not opened.
pub fn files(dir: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let prefixes: Vec<String> = roots.iter().map(|r| project_name(r)).collect();
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        // A prefix match can also be a sibling (`/a/b` and `/a/b-c`); each line's `cwd` decides.
        if prefixes
            .iter()
            .any(|p| name == *p || name.starts_with(&format!("{p}-")))
        {
            out.extend(jsonl_files(&e.path()));
        }
    }
    out
}

/// A working directory's project directory name: every character but ASCII letters and digits
/// becomes `-`.
fn project_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn parse(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let (Some(session), Some(ts)) = (
        v["sessionId"].as_str(),
        v["timestamp"].as_str().and_then(time::parse),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(cwd) = v["cwd"].as_str() {
        out.push(Event::Session {
            id: session.to_string(),
            cwd: cwd.into(),
            ts,
        });
    }
    for b in v["message"]["content"].as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("tool_use") => out.extend(call(session, ts, b).map(Event::Call)),
            Some("tool_result") => out.extend(b["tool_use_id"].as_str().map(|id| Event::Done {
                id: id.to_string(),
                ts,
                text: text_of(&b["content"]),
                is_error: b["is_error"].as_bool().unwrap_or(false),
            })),
            _ => {}
        }
    }
    out
}

fn call(session: &str, ts: i64, b: &Value) -> Option<Call> {
    let input = &b["input"];
    let (kind, query, path) = match b["name"].as_str()? {
        "Grep" => (Kind::Grep, None, None),
        "Glob" => (Kind::Glob, None, None),
        "Read" => (
            Kind::Read,
            None,
            input["file_path"].as_str().map(String::from),
        ),
        "Bash" if input["command"].as_str().is_some_and(is_shell_search) => {
            (Kind::Grep, None, None)
        }
        // `mcp__lodestone__…`, or `mcp__plugin_lodestone_lodestone__…` from the plugin.
        name => {
            let tool = name
                .strip_prefix("mcp__")
                .filter(|n| n.contains("lodestone"))?
                .rsplit("__")
                .next()?;
            let kind = Kind::lodestone(tool)?;
            (kind, Some(query_of(kind, input)), None)
        }
    };
    Some(Call {
        id: b["id"].as_str()?.to_string(),
        session: session.to_string(),
        kind,
        ts,
        query,
        path,
        done: None,
    })
}

/// Whether a shell command runs a text search (`rg`, `grep`, `git grep`, …) in any of its parts.
fn is_shell_search(cmd: &str) -> bool {
    cmd.split(['|', ';', '&', '\n']).any(|part| {
        let mut words = part
            .split_whitespace()
            .skip_while(|w| w.contains('=') && !w.starts_with('-'));
        match words.next().map(|w| w.rsplit('/').next().unwrap_or(w)) {
            Some("rg" | "grep" | "egrep" | "ag" | "ack") => true,
            Some("git") => words.next() == Some("grep"),
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_names_match_claude_code() {
        assert_eq!(
            project_name(Path::new("/Users/me/src/lode.stone/.claude/worktrees/x")),
            "-Users-me-src-lode-stone--claude-worktrees-x"
        );
    }

    #[test]
    fn shell_searches() {
        assert!(is_shell_search("cd repo && rg -n foo src"));
        assert!(is_shell_search("LC_ALL=C grep -r foo ."));
        assert!(is_shell_search("git grep -n foo"));
        assert!(!is_shell_search("cargo nextest run"));
        assert!(!is_shell_search("git log --grep foo"));
    }
}
