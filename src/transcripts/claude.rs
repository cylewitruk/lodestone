//! Claude Code transcripts: `<projects>/<project>/<session>.jsonl`, plus subagents in
//! `<session>/subagents/*.jsonl` under their parent's session id. Each line carries its session,
//! working directory and timestamp. Tool calls are `tool_use` blocks and their results
//! `tool_result` blocks. A `Bash` call counts as the searches and file reads its command makes
//! (see [`shell`]). Attachments list the tools a session has: `prompt_snapshot` those loaded,
//! `deferred_tools_delta` those deferred behind ToolSearch. A `hook_additional_context` attachment
//! holding lodestone's nudge counts as a [`Kind::Nudge`] call, one holding its edit hint as a
//! [`Kind::EditHint`] call in the session's repository.

use std::path::Path;

use serde_json::Value;

use super::{Call, Event, Kind, Target, query_of, shell_calls, text_of, time};
use crate::agents;

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
    let cwd = v["cwd"].as_str();
    if let Some(cwd) = cwd {
        out.push(Event::Session {
            id: session.to_string(),
            cwd: Some(cwd.into()),
            ts,
        });
    }
    out.extend(tools(&v["attachment"]).map(|loaded| Event::Tools {
        id: session.to_string(),
        loaded,
    }));
    out.extend(hinted(&v["attachment"]).map(|(call, kind)| {
        Event::Call(Call {
            id: format!("{call}:n"),
            session: session.to_string(),
            kind,
            ts,
            query: None,
            path: None,
            target: (kind == Kind::Nudge).then(|| Target::Searches(call.to_string())),
            done: None,
        })
    }));
    for b in v["message"]["content"].as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("tool_use") => out.extend(
                calls(session, ts, cwd.map(Path::new), b)
                    .into_iter()
                    .map(Event::Call),
            ),
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

/// The calls a `tool_use` block counts as, made in `cwd`.
fn calls(session: &str, ts: i64, cwd: Option<&Path>, b: &Value) -> Vec<Call> {
    let (Some(id), Some(name)) = (b["id"].as_str(), b["name"].as_str()) else {
        return Vec::new();
    };
    let input = &b["input"];
    let call = |id: String, kind, query, path: Option<String>, target| Call {
        id,
        session: session.to_string(),
        kind,
        ts,
        query,
        path,
        target,
        done: None,
    };
    let path_of = |key: &str| input[key].as_str().map(|p| Target::Path(p.into()));
    let one = |kind, query, path, target| vec![call(id.to_string(), kind, query, path, target)];
    match name {
        "Grep" => one(Kind::Grep, None, None, path_of("path")),
        "Glob" => one(Kind::Glob, None, None, path_of("path")),
        "Read" => one(
            Kind::Read,
            None,
            input["file_path"].as_str().map(String::from),
            path_of("file_path"),
        ),
        "Bash" => shell_calls(
            id,
            session,
            ts,
            input["command"].as_str().unwrap_or_default(),
            cwd,
        ),
        name => lodestone_tool(name)
            .map(|kind| {
                let target = input["repo"].as_str().map(|r| Target::Repo(r.into()));
                one(kind, Some(query_of(kind, input)), None, target)
            })
            .unwrap_or_default(),
    }
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

/// The call a `PreToolUse` hook's context attachment is about, and which of lodestone's contexts
/// it holds: the nudge or the edit hint.
fn hinted(attachment: &Value) -> Option<(&str, Kind)> {
    let a = attachment;
    if a["type"] != "hook_additional_context" || a["hookEvent"] != "PreToolUse" {
        return None;
    }
    let kind = a["content"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find_map(|c| {
            if c.starts_with(agents::NUDGE_OPENING) {
                Some(Kind::Nudge)
            } else if c.starts_with(agents::EDIT_HINT_OPENING) {
                Some(Kind::EditHint)
            } else {
                None
            }
        })?;
    Some((a["toolUseID"].as_str()?, kind))
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

#[cfg(test)]
mod tests {
    use super::*;

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
