//! Antigravity transcripts: `<app data>/brain/<conversation>/.system_generated/logs/
//! transcript_full.jsonl` (`transcript.jsonl` truncates tool results), one line per step. A model
//! step lists its tool calls, and the steps after it carry their results, one each, in order, so
//! each call is named by the step its result will be, and a result matches its call by its own. The
//! workspace a conversation runs in is only in the app's `conversation_summaries.db`. A
//! `run_command` call counts as the searches and file reads its command makes (see [`shell`]).
//!
//! [`shell`]: crate::shell

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::{Call, Event, Kind, Target, query_of, shell_calls, time};
use crate::file_url;

/// The conversation a transcript belongs to, and its workspace.
#[derive(Clone, Debug, Default)]
pub struct Context {
    session: String,
    workspace: Option<PathBuf>,
}

impl Context {
    /// The context of transcript `path`, in a conversation whose workspace `workspaces` may name.
    pub fn new(path: &Path, workspaces: &HashMap<String, PathBuf>) -> Self {
        // `<conversation>/.system_generated/logs/transcript_full.jsonl`
        let session = path
            .ancestors()
            .nth(3)
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Context {
            workspace: workspaces.get(&session).cloned(),
            session,
        }
    }

    /// The workspace recorded for the conversation, if any.
    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }
}

/// Every conversation's transcript under app data directory `dir`.
pub fn transcripts(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir.join("brain"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            e.path()
                .join(".system_generated/logs/transcript_full.jsonl")
        })
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

/// Each conversation's first workspace, from app data directory `dir`; empty when unreadable.
pub fn workspaces(dir: &Path) -> HashMap<String, PathBuf> {
    let read = || -> rusqlite::Result<HashMap<String, PathBuf>> {
        let conn = Connection::open_with_flags(
            dir.join("conversation_summaries.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut stmt =
            conn.prepare("SELECT conversation_id, workspace_uris FROM conversation_summaries")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows
            .flatten()
            .filter_map(|(id, uris)| {
                let uris: Vec<String> = serde_json::from_str(&uris).ok()?;
                Some((id, file_url::local_path(uris.first()?)))
            })
            .collect())
    };
    read().unwrap_or_default()
}

pub fn parse(line: &str, ctx: &Context) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(ts) = v["created_at"].as_str().and_then(time::parse) else {
        return Vec::new();
    };
    let step = v["step_index"].as_u64().unwrap_or_default();
    let mut out = Vec::new();
    if step == 0 {
        out.push(Event::Session {
            id: ctx.session.clone(),
            cwd: ctx.workspace.clone(),
            ts,
        });
    }
    match v["type"].as_str() {
        Some("PLANNER_RESPONSE") => {
            for (i, c) in (step + 1..).zip(v["tool_calls"].as_array().into_iter().flatten()) {
                out.extend(
                    calls(ctx, &i.to_string(), ts, c)
                        .into_iter()
                        .map(Event::Call),
                );
            }
        }
        Some("GENERIC") => {
            let text = v["content"].as_str().unwrap_or_default();
            out.push(Event::Done {
                id: step.to_string(),
                ts: completed_at(text).unwrap_or(ts),
                text: text.to_string(),
                is_error: v["status"] == "ERROR",
            });
        }
        _ => {}
    }
    out
}

/// The calls a tool call counts as. A command with no directory of its own runs in the workspace.
fn calls(ctx: &Context, id: &str, ts: i64, c: &Value) -> Vec<Call> {
    let session = ctx.session.as_str();
    let args = &c["args"];
    let one = |kind, query, path: Option<&str>, target| {
        vec![Call {
            id: id.to_string(),
            session: session.to_string(),
            kind,
            ts,
            query,
            path: path.map(String::from),
            target,
            done: None,
        }]
    };
    let arg = |key: &str| args[key].as_str();
    let path_of = |key: &str| arg(key).map(|p| Target::Path(p.into()));
    match c["name"].as_str().unwrap_or_default() {
        "grep_search" => one(Kind::Grep, None, None, path_of("SearchPath")),
        "find_by_name" => one(Kind::Glob, None, None, path_of("SearchDirectory")),
        "list_dir" => one(Kind::Glob, None, None, path_of("DirectoryPath")),
        "view_file" => one(
            Kind::Read,
            None,
            arg("AbsolutePath"),
            path_of("AbsolutePath"),
        ),
        "read_file" => one(Kind::Read, None, arg("Path"), path_of("Path")),
        "run_command" => shell_calls(
            id,
            session,
            ts,
            arg("CommandLine").unwrap_or_default(),
            arg("Cwd").map(Path::new).or(ctx.workspace.as_deref()),
        ),
        "call_mcp_tool" => arg("ServerName")
            .filter(|s| s.contains("lodestone"))
            .and_then(|_| Kind::lodestone(arg("ToolName")?))
            .map(|kind| {
                let input = &args["Arguments"];
                let target = input["repo"].as_str().map(|r| Target::Repo(r.into()));
                one(kind, Some(query_of(kind, input)), None, target)
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// When a result step says its call completed: its last `Completed At:` line.
fn completed_at(text: &str) -> Option<i64> {
    text.lines()
        .filter_map(|l| l.strip_prefix("Completed At: "))
        .next_back()
        .and_then(time::parse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(index: u64, kind: &str, extra: Value) -> String {
        let mut v = json!({ "step_index": index, "type": kind, "status": "DONE",
            "created_at": "2026-11-24T21:48:44Z" });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v.to_string()
    }

    #[test]
    fn each_call_is_named_by_the_step_of_its_result() {
        let ctx = Context {
            session: "c".into(),
            workspace: Some("/w".into()),
        };
        let first = parse(&step(0, "USER_INPUT", json!({})), &ctx);
        std::assert_matches!(&first[..], [Event::Session { id, cwd: Some(_), .. }] if id == "c");
        let calls = parse(
            &step(
                1,
                "PLANNER_RESPONSE",
                json!({ "tool_calls": [
                    { "name": "view_file", "args": { "AbsolutePath": "/w/a.rs" } },
                    { "name": "call_mcp_tool", "args": { "ServerName": "lodestone_lodestone",
                        "ToolName": "semantic_code_search", "Arguments": { "query": "q" } } },
                    { "name": "call_mcp_tool", "args": { "ServerName": "other",
                        "ToolName": "semantic_code_search", "Arguments": {} } },
                ] }),
            ),
            &ctx,
        );
        std::assert_matches!(
            &calls[..],
            [Event::Call(read), Event::Call(search)]
                if read.id == "2" && read.kind == Kind::Read
                    && read.path.as_deref() == Some("/w/a.rs")
                    && search.id == "3" && search.query.as_deref() == Some("q")
        );
        let result = step(
            3,
            "GENERIC",
            json!({ "status": "ERROR",
                "content": "Created At: 2026-11-24T22:48:48+01:00\nCompleted At: 2026-11-24T22:48:50+01:00\nsrc/a.rs:1:4  lines 1-2  fn a()" }),
        );
        std::assert_matches!(
            &parse(&result, &ctx)[..],
            [Event::Done { id, ts, is_error: true, .. }]
                if id == "3" && *ts == time::parse("2026-11-24T21:48:50Z").unwrap()
        );
    }

    #[test]
    fn the_conversation_is_named_by_its_directory() {
        let path = Path::new("/d/brain/abc/.system_generated/logs/transcript_full.jsonl");
        let workspaces = HashMap::from([("abc".to_string(), PathBuf::from("/w"))]);
        let ctx = Context::new(path, &workspaces);
        assert_eq!(ctx.session, "abc");
        assert_eq!(ctx.workspace, Some(PathBuf::from("/w")));
    }
}
