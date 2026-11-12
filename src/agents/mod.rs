//! Integration with coding-agent harnesses (Claude Code, Codex): the lodestone skill, the
//! session reminder hooks, the `lode mcp` stdio relay, and installing all three.

mod install;
mod relay;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::json;

pub use install::{Harness, Scope, Step, Target, apply, install, uninstall};
pub use relay::run as run_relay;

use crate::api::types::SessionRequest;
use crate::client::{Client, start_daemon};
use crate::config::Config;
use crate::registry;

/// How long the hook's lease keeps the session's worktree watched before the relay's takes over.
const HOOK_LEASE_SECS: u64 = 300;

/// Markers older than this are removed when a new one is written.
const MARKER_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// The lodestone skill, shared by every harness (`.agents/skills/lodestone/SKILL.md`).
pub const SKILL: &str = include_str!("../../plugin/skills/lodestone/SKILL.md");

/// Session context added in repositories the daemon serves.
pub const REMINDER: &str = "Before writing any new function, type or macro, when reviewing a diff that adds functions, when consolidating duplicated code, and when locating the code behind a described behaviour, look for existing code that already does it, and reuse what fits instead of writing a new version. This repository is indexed by lodestone: its `semantic_code_search` tool finds code by what it does, which Grep cannot when you do not know the name, and its `similar_code_search` tool finds code that does the same job as given functions; pass it every function a diff adds or a refactor moves, in one call (see the lodestone skill). Use Grep for exact strings.";

/// A harness hook that adds the reminder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    /// Every session start, resume and compaction.
    SessionStart,
    /// Each prompt; adds the reminder only when the session has not had it, for harnesses that
    /// skip SessionStart hooks at startup (Claude Code in the desktop app).
    UserPromptSubmit,
}

impl HookEvent {
    /// The harness's name for the event.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
        }
    }

    /// The `lode` subcommand that answers it.
    pub fn args(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "hook session-start",
            HookEvent::UserPromptSubmit => "hook prompt-submit",
        }
    }
}

/// The hook's output for `event` in session `session` (the harness's id) in `dir`: the reminder
/// as hook JSON when `dir` is in a registered worktree, otherwise `None`.
///
/// Each session is checked once, at its first hook, which leaves a marker: `UserPromptSubmit`
/// stays silent once one exists, or when the harness gives no session id. `SessionStart` always
/// checks, since a compacted session has lost the reminder.
pub fn hook(
    event: HookEvent,
    dir: &Path,
    session: Option<&str>,
    config: &Config,
    config_path: Option<&Path>,
) -> Option<String> {
    let marker = session.and_then(|id| marker_path(config, id));
    if event == HookEvent::UserPromptSubmit && marker.as_ref().is_none_or(|m| m.exists()) {
        return None;
    }
    let served = serves(dir, config, config_path);
    if let Some(m) = &marker
        && let Err(e) = write_marker(m, event)
    {
        tracing::warn!("{e:#}");
    }
    served.then(|| {
        json!({
            "hookSpecificOutput": {
                "hookEventName": event.name(),
                "additionalContext": REMINDER,
            }
        })
        .to_string()
    })
}

/// Marks a session as checked; its contents name the hook that checked it.
fn marker_path(config: &Config, session: &str) -> Option<PathBuf> {
    let id: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    (!id.is_empty()).then(|| config.data_dir().join("hook-sessions").join(id))
}

fn write_marker(path: &Path, event: HookEvent) -> anyhow::Result<()> {
    let dir = path.parent().expect("marker has a parent");
    fs::create_dir_all(dir)?;
    fs::write(path, event.name())?;
    let now = SystemTime::now();
    for entry in fs::read_dir(dir)?.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > MARKER_MAX_AGE);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Whether the daemon serves `dir`.
///
/// A running daemon is asked, and starts catching that worktree up at once. With no daemon, the
/// registry decides, and an on-demand daemon is started in the background so the model loads
/// while the session starts.
fn serves(dir: &Path, config: &Config, config_path: Option<&Path>) -> bool {
    match Client::connect(&config.endpoint()) {
        Some(c) => {
            let lease = SessionRequest {
                id: format!("hook-{}", std::process::id()),
                cwd: dir.to_string_lossy().into_owned(),
                ttl_secs: HOOK_LEASE_SECS,
            };
            match c.session(&lease) {
                Ok(r) => r.repo.is_some(),
                // A daemon without sessions (an older build): ask for its worktrees instead.
                Err(_) => c.repos().is_ok_and(|repos| {
                    let roots: Vec<_> = repos.into_iter().map(|r| r.root.into()).collect();
                    registry::root_of(&roots, dir).is_some()
                }),
            }
        }
        None => {
            let served = matches!(
                registry::place(&registry::registered_roots(config), dir),
                registry::Placement::Registered(_) | registry::Placement::NewWorktree(_)
            );
            if served && let Err(e) = start_daemon(config, config_path) {
                tracing::warn!("{e:#}");
            }
            served
        }
    }
}
