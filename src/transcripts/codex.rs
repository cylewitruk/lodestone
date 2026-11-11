//! Codex transcripts: `<sessions>/YYYY/MM/DD/rollout-*.jsonl`. The first line, `session_meta`,
//! names the session (a subagent's names its parent thread) and its working directory. Finished
//! tool calls are `item_completed` events: shell commands carry Codex's own classification
//! (`parsed_cmd`: read, search, list_files), and MCP calls their server, tool and result.

use std::path::PathBuf;

use serde_json::Value;

use super::{Call, Event, Kind, Target, query_of, text_of, time};

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
    let call = |id: String, kind, query, path, target, done| Call {
        id,
        session: session.to_string(),
        kind,
        ts: start,
        query,
        path,
        target,
        done,
    };
    match item["type"].as_str() {
        Some("CommandExecution") => {
            let cwd = item["cwd"].as_str().map(local_path);
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
                    // Relative to the command's directory; a search may name no path.
                    let at = match (&cwd, c["path"].as_str()) {
                        (Some(cwd), Some(p)) => Some(cwd.join(p)),
                        (Some(cwd), None) => Some(cwd.clone()),
                        (None, p) => p.map(PathBuf::from),
                    };
                    let path = at
                        .as_ref()
                        .filter(|_| kind == Kind::Read)
                        .map(|p| p.to_string_lossy().into_owned());
                    Some(Event::Call(call(
                        format!("{id}#{i}"),
                        kind,
                        None,
                        path,
                        at.map(Target::Path),
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
            let args = &item["arguments"];
            let target = args["repo"].as_str().map(|r| Target::Repo(r.into()));
            vec![Event::Call(call(
                id.to_string(),
                kind,
                Some(query_of(kind, args)),
                None,
                target,
                Some(done),
            ))]
        }
        _ => Vec::new(),
    }
}

/// A path, given plainly or as a `file://` URL (newer Codex versions record command directories
/// as URLs).
fn local_path(s: &str) -> PathBuf {
    let Some(rest) = s.strip_prefix("file://") else {
        return PathBuf::from(s);
    };
    let hex = |c: Option<u8>| c.and_then(|c| (c as char).to_digit(16));
    let mut bytes = Vec::with_capacity(rest.len());
    let mut it = rest.bytes();
    while let Some(b) = it.next() {
        if b != b'%' {
            bytes.push(b);
            continue;
        }
        let (hi, lo) = (it.next(), it.next());
        match (hex(hi), hex(lo)) {
            (Some(h), Some(l)) => bytes.push((h * 16 + l) as u8),
            _ => bytes.extend([Some(b), hi, lo].into_iter().flatten()),
        }
    }
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_directories_as_urls() {
        assert_eq!(local_path("/a/b"), PathBuf::from("/a/b"));
        assert_eq!(
            local_path("file:///a/my%20repo"),
            PathBuf::from("/a/my repo")
        );
    }
}
