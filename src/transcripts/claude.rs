//! Claude Code transcripts: `<projects>/<project>/<session>.jsonl`, plus subagents in
//! `<session>/subagents/*.jsonl` under their parent's session id. Each line carries its session,
//! working directory and timestamp. Tool calls are `tool_use` blocks and their results
//! `tool_result` blocks. Attachments list the tools a session has: `prompt_snapshot` those loaded,
//! `deferred_tools_delta` those deferred behind ToolSearch.

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
    out.extend(tools(&v["attachment"]).map(|loaded| Event::Tools {
        id: session.to_string(),
        loaded,
    }));
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
        name => {
            let kind = lodestone_tool(name)?;
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

/// The lodestone search an MCP tool name calls: `mcp__lodestone__…`, or
/// `mcp__plugin_lodestone_lodestone__…` from the plugin.
fn lodestone_tool(name: &str) -> Option<Kind> {
    let tool = name
        .strip_prefix("mcp__")
        .filter(|n| n.contains("lodestone"))?
        .rsplit("__")
        .next()?;
    Kind::lodestone(tool)
}

/// Whether an attachment lists a lodestone search tool as loaded (`true`) or deferred (`false`).
fn tools(attachment: &Value) -> Option<bool> {
    let (names, loaded): (Vec<&str>, bool) = match attachment["type"].as_str()? {
        "prompt_snapshot" => (
            attachment["tools"]
                .as_array()?
                .iter()
                .filter_map(|t| t["name"].as_str())
                .collect(),
            true,
        ),
        "deferred_tools_delta" => (
            attachment["addedNames"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect(),
            false,
        ),
        _ => return None,
    };
    names
        .into_iter()
        .any(|n| lodestone_tool(n).is_some())
        .then_some(loaded)
}

/// Whether a shell command runs a text search (`rg`, `grep`, `git grep`, …) over files. A search
/// fed by a pipe filters another command's output and does not count.
fn is_shell_search(cmd: &str) -> bool {
    cmd.replace("||", ";").split([';', '&', '\n']).any(|part| {
        let mut words = part
            .split('|')
            .next()
            .unwrap_or_default()
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
        assert!(is_shell_search("grep -rn 'a\\|b' src | head"));
        assert!(is_shell_search("test -d src || rg foo"));
        assert!(!is_shell_search(
            "cargo nextest run 2>&1 | grep -E 'FAIL|ok'"
        ));
        assert!(!is_shell_search("git show HEAD:a.rs | grep -n foo"));
    }

    #[test]
    fn attachments_say_how_the_search_tools_are_listed() {
        let event = |attachment: Value| {
            let line = serde_json::json!({ "sessionId": "s", "timestamp": "2026-11-11T10:00:00.000Z",
                "attachment": attachment });
            parse(&line.to_string())
        };
        let tools = |loaded| {
            vec![Event::Tools {
                id: "s".into(),
                loaded,
            }]
        };
        let loaded = serde_json::json!({ "type": "prompt_snapshot",
            "tools": [{ "name": "Bash" }, { "name": "mcp__lodestone__semantic_code_search" }] });
        assert_eq!(event(loaded), tools(true));
        let deferred = serde_json::json!({ "type": "deferred_tools_delta",
            "addedNames": ["mcp__plugin_lodestone_lodestone__similar_code_search"] });
        assert_eq!(event(deferred), tools(false));
        let neither = serde_json::json!({ "type": "deferred_tools_delta",
            "addedNames": ["mcp__lodestone__status"] });
        assert_eq!(event(neither), vec![]);
    }
}
