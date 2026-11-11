//! Codex transcripts: `<sessions>/YYYY/MM/DD/rollout-*.jsonl`. The first line, `session_meta`,
//! names the session (a subagent's names its parent thread) and its working directory. Finished
//! tool calls are `item_completed` events: shell commands carry Codex's own classification
//! (`parsed_cmd`: read, search, list_files), and MCP calls their server, tool and result.

use std::path::Path;

use serde_json::Value;

use super::{Call, Event, Kind, query_of, text_of, time};

/// Per-file state carried between lines (and runs): the session the file belongs to.
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub session: Option<String>,
}

pub fn parse(line: &str, ctx: &mut Context) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let p = &v["payload"];
    if v["type"] == "session_meta" {
        let (Some(id), Some(cwd)) = (
            p["parent_thread_id"].as_str().or(p["id"].as_str()),
            p["cwd"].as_str(),
        ) else {
            return Vec::new();
        };
        ctx.session = Some(id.to_string());
        let ts = p["timestamp"]
            .as_str()
            .or(v["timestamp"].as_str())
            .and_then(time::parse)
            .unwrap_or(0);
        return vec![Event::Session {
            id: id.to_string(),
            cwd: cwd.into(),
            ts,
        }];
    }
    if p["type"] != "item_completed" {
        return Vec::new();
    }
    let Some(session) = ctx.session.as_deref() else {
        return Vec::new();
    };
    let item = &p["item"];
    let (Some(id), Some(start)) = (item["id"].as_str(), p["started_at_ms"].as_i64()) else {
        return Vec::new();
    };
    let end = p["completed_at_ms"].as_i64().unwrap_or(start);
    let call = |id: String, kind, query, path, done| Call {
        id,
        session: session.to_string(),
        kind,
        ts: start,
        query,
        path,
        done,
    };
    match item["type"].as_str() {
        Some("CommandExecution") => {
            let cwd = item["cwd"].as_str().map(Path::new);
            item["parsed_cmd"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(|(i, c)| {
                    let kind = match c["type"].as_str()? {
                        "read" => Kind::Read,
                        "search" => Kind::Grep,
                        "list_files" => Kind::Glob,
                        _ => return None,
                    };
                    let path = (kind == Kind::Read)
                        .then(|| c["path"].as_str())
                        .flatten()
                        .map(|p| match cwd {
                            Some(cwd) => cwd.join(p).to_string_lossy().into_owned(),
                            None => p.to_string(),
                        });
                    Some(Event::Call(call(
                        format!("{id}#{i}"),
                        kind,
                        None,
                        path,
                        None,
                    )))
                })
                .collect()
        }
        Some("McpToolCall") => {
            let kind = item["server"]
                .as_str()
                .filter(|s| s.contains("lodestone"))
                .and_then(|_| Kind::lodestone(item["tool"].as_str()?));
            let Some(kind) = kind else {
                return Vec::new();
            };
            let result = &item["result"];
            let is_error =
                result["isError"].as_bool().unwrap_or(false) || item["status"] == "failed";
            let done = (end, text_of(&result["content"]), is_error);
            vec![Event::Call(call(
                id.to_string(),
                kind,
                Some(query_of(kind, &item["arguments"])),
                None,
                Some(done),
            ))]
        }
        _ => Vec::new(),
    }
}
