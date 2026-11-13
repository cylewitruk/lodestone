//! Integration with coding-agent harnesses (Claude Code, Codex): the lodestone skill, the
//! session reminder hooks, the `lode mcp` stdio relay, and installing all three.

mod install;
mod marker;
mod relay;

use std::path::{Path, PathBuf};

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

/// The reminder's advice, ahead of what lodestone serves.
const ADVICE: &str = "Before writing any new function, type or macro, when reviewing a diff that adds functions, when consolidating duplicated code, and when locating the code behind a described behaviour, look for existing code that already does it, and reuse what fits instead of writing a new version.";

/// The reminder's description of the tools, after what lodestone serves.
const TOOLS: &str = "The `semantic_code_search` tool finds code by what it does, which Grep cannot when you do not know the name, and the `similar_code_search` tool finds code that does the same job as given functions; pass it every function a diff adds or a refactor moves, in one call (see the lodestone skill). Use Grep for exact strings.";

/// What lodestone serves a session's directory.
#[derive(Debug, PartialEq)]
enum Served {
    /// The worktree the directory is in.
    Worktree,
    /// Registered worktrees (name, root) inside the directory, which is in none.
    Inside(Vec<(String, PathBuf)>),
}

/// Session context for a directory lodestone serves.
fn reminder(served: &Served) -> String {
    match served {
        Served::Worktree => format!("{ADVICE} This repository is indexed by lodestone. {TOOLS}"),
        Served::Inside(repos) => format!(
            "{ADVICE} This folder is not itself indexed by lodestone, but these repositories in it are: {}. Pass the one you work in as `repo` (its name, or the path of any of its git worktrees) to lodestone's tools. {TOOLS}",
            registry::listing(repos)
        ),
    }
}

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
/// Every hook keeps the session's marker (see [`marker`]) current. `UserPromptSubmit` adds the
/// reminder only when it writes a session's first marker, so never without a session id;
/// `SessionStart` always adds it, since a compacted session has lost it.
pub fn hook(
    event: HookEvent,
    dir: &Path,
    session: Option<&str>,
    config: &Config,
    config_path: Option<&Path>,
) -> Option<String> {
    let path = session.and_then(|id| marker::path(&config.data_dir(), id));
    let previous = path.as_deref().and_then(marker::read);
    if let Some(p) = &path
        && (event == HookEvent::SessionStart
            || previous
                .as_ref()
                .is_none_or(|m| m.cwd.as_deref() != Some(dir)))
    {
        let m = marker::Marker {
            event: event.name().into(),
            cwd: Some(dir.to_path_buf()),
        };
        if let Err(e) = marker::write(p, &m) {
            tracing::warn!("{e:#}");
        }
    }
    let remind = match event {
        HookEvent::SessionStart => true,
        HookEvent::UserPromptSubmit => path.is_some() && previous.is_none(),
    };
    let served = remind.then(|| serves(dir, config, config_path)).flatten()?;
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": event.name(),
                "additionalContext": reminder(&served),
            }
        })
        .to_string(),
    )
}

/// What the daemon serves `dir`, if anything.
///
/// A running daemon is asked, and starts catching that worktree up at once. With no daemon, the
/// registry decides, and an on-demand daemon is started in the background so the model loads
/// while the session starts.
fn serves(dir: &Path, config: &Config, config_path: Option<&Path>) -> Option<Served> {
    let inside = |repos: Vec<(String, PathBuf)>| {
        let repos = registry::inside(repos, dir);
        (!repos.is_empty()).then_some(Served::Inside(repos))
    };
    match Client::connect(&config.endpoint()) {
        Some(c) => {
            let lease = SessionRequest {
                id: format!("hook-{}", std::process::id()),
                cwd: dir.to_string_lossy().into_owned(),
                ttl_secs: HOOK_LEASE_SECS,
            };
            let worktree = match c.session(&lease) {
                Ok(r) => r.repo.is_some(),
                // A daemon without sessions (an older build): ask for its worktrees instead.
                Err(_) => c.repos().is_ok_and(|repos| {
                    let roots: Vec<_> = repos.into_iter().map(|r| r.root.into()).collect();
                    registry::root_of(&roots, dir).is_some()
                }),
            };
            if worktree {
                return Some(Served::Worktree);
            }
            let repos = c.repos().ok()?;
            inside(repos.into_iter().map(Into::into).collect())
        }
        None => {
            let served = match registry::place(&registry::registered_roots(config), dir) {
                registry::Placement::Registered(_) | registry::Placement::NewWorktree(_) => {
                    Some(Served::Worktree)
                }
                _ => inside(registry::registered(config)),
            };
            if served.is_some()
                && let Err(e) = start_daemon(config, config_path)
            {
                tracing::warn!("{e:#}");
            }
            served
        }
    }
}
