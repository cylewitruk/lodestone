//! Integration with coding-agent harnesses (Claude Code, Codex): the lodestone skill, the
//! SessionStart reminder, the `lode mcp` stdio relay, and installing all three.

mod install;
mod relay;

use std::path::Path;

use serde_json::json;

pub use install::{Harness, Scope, Step, Target, apply, install, uninstall};
pub use relay::run as run_relay;

use crate::api::types::SessionRequest;
use crate::client::{Client, start_daemon};
use crate::config::Config;
use crate::registry;

/// How long the hook's lease keeps the session's worktree watched before the relay's takes over.
const HOOK_LEASE_SECS: u64 = 300;

/// The lodestone skill, shared by every harness (`.agents/skills/lodestone/SKILL.md`).
pub const SKILL: &str = include_str!("../../plugin/skills/lodestone/SKILL.md");

/// Session context added in repositories the daemon serves.
pub const REMINDER: &str = "Before writing any new function, type or macro, when reviewing a diff that adds functions, when consolidating duplicated code, and when locating the code behind a described behaviour, look for existing code that already does it, and reuse what fits instead of writing a new version. This repository is indexed by lodestone: its `semantic_code_search` tool finds code by what it does, which Grep cannot when you do not know the name, and its `similar_code_search` tool finds code that does the same job as a given function; call it once for each function a diff adds or a refactor moves (see the lodestone skill). Use Grep for exact strings.";

/// The SessionStart hook's output for a session in `dir`: the reminder as hook JSON when `dir` is
/// in a registered worktree, otherwise `None`.
///
/// A running daemon is asked, and starts catching that worktree up at once. With no daemon, the
/// registry decides, and an on-demand daemon is started in the background so the model loads
/// while the session starts.
pub fn session_start(dir: &Path, config: &Config, config_path: Option<&Path>) -> Option<String> {
    let served = match Client::connect(&config.endpoint()) {
        Some(c) => {
            let lease = SessionRequest {
                id: format!("hook-{}", std::process::id()),
                cwd: dir.to_string_lossy().into_owned(),
                ttl_secs: HOOK_LEASE_SECS,
            };
            match c.session(&lease) {
                Ok(r) => r.repo.is_some(),
                // A daemon without sessions (an older build): ask for its worktrees instead.
                Err(_) => {
                    let roots: Vec<_> =
                        c.repos().ok()?.into_iter().map(|r| r.root.into()).collect();
                    registry::root_of(&roots, dir).is_some()
                }
            }
        }
        None => {
            let served = registry::root_of(&registry::registered_roots(config), dir).is_some();
            if served && let Err(e) = start_daemon(config, config_path) {
                tracing::warn!("{e:#}");
            }
            served
        }
    };
    served.then(|| {
        json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": REMINDER,
            }
        })
        .to_string()
    })
}
