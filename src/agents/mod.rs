//! Integration with coding-agent harnesses (Claude Code, Codex): the lodestone skill, the
//! SessionStart reminder, and installing both with the MCP server.

mod install;

use std::path::Path;

use serde_json::json;

pub use install::{Harness, Scope, Step, Target, apply, install, uninstall};

use crate::client::Client;

/// The lodestone skill, shared by every harness (`.agents/skills/lodestone/SKILL.md`).
pub const SKILL: &str = include_str!("../../plugin/skills/lodestone/SKILL.md");

/// Session context added in repositories the daemon serves.
pub const REMINDER: &str = "Before writing any new function, type or macro, when reviewing a diff that adds functions, when consolidating duplicated code, and when locating the code behind a described behaviour, look for existing code that already does it, and reuse what fits instead of writing a new version. This repository is indexed by lodestone: its `semantic_code_search` tool finds code by what it does, which Grep cannot when you do not know the name, and its `similar_code_search` tool finds code that does the same job as a given function; call it once for each function a diff adds or a refactor moves (see the lodestone skill). Use Grep for exact strings.";

/// The SessionStart hook's output for a session in `dir`: the reminder as hook JSON when the
/// daemon on `port` serves `dir`, otherwise `None`.
pub fn session_start(dir: &Path, port: u16) -> Option<String> {
    let dir = dir.canonicalize().ok()?;
    let repos = Client::connect(port)?.repos().ok()?;
    repos.iter().any(|r| dir.starts_with(&r.root)).then(|| {
        json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": REMINDER,
            }
        })
        .to_string()
    })
}
