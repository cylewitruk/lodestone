//! Claude Code transcripts: `<projects>/<project>/<session>.jsonl`, plus subagents in
//! `<session>/subagents/*.jsonl` under their parent's session id. Each line carries its session,
//! working directory and timestamp. Tool calls are `tool_use` blocks and their results
//! `tool_result` blocks.

use serde_json::Value;

use super::{Call, Event, Kind, Target, query_of, text_of, time};

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
    let path_of = |key: &str| input[key].as_str().map(|p| Target::Path(p.into()));
    let (kind, query, path, target) = match b["name"].as_str()? {
        "Grep" => (Kind::Grep, None, None, path_of("path")),
        "Glob" => (Kind::Glob, None, None, path_of("path")),
        "Read" => (
            Kind::Read,
            None,
            input["file_path"].as_str().map(String::from),
            path_of("file_path"),
        ),
        "Bash" if input["command"].as_str().is_some_and(is_shell_search) => {
            (Kind::Grep, None, None, None)
        }
        // `mcp__lodestone__…`, or `mcp__plugin_lodestone_lodestone__…` from the plugin.
        name => {
            let tool = name
                .strip_prefix("mcp__")
                .filter(|n| n.contains("lodestone"))?
                .rsplit("__")
                .next()?;
            let kind = Kind::lodestone(tool)?;
            let target = input["repo"].as_str().map(|r| Target::Repo(r.into()));
            (kind, Some(query_of(kind, input)), None, target)
        }
    };
    Some(Call {
        id: b["id"].as_str()?.to_string(),
        session: session.to_string(),
        kind,
        ts,
        query,
        path,
        target,
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
    fn shell_searches() {
        assert!(is_shell_search("cd repo && rg -n foo src"));
        assert!(is_shell_search("LC_ALL=C grep -r foo ."));
        assert!(is_shell_search("git grep -n foo"));
        assert!(!is_shell_search("cargo nextest run"));
        assert!(!is_shell_search("git log --grep foo"));
    }
}
