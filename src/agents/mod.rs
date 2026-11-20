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
use crate::shell;

/// How long the hook's lease keeps the session's worktree watched before the relay's takes over.
const HOOK_LEASE_SECS: u64 = 300;

/// The lodestone skill, shared by every harness (`.agents/skills/lodestone/SKILL.md`).
pub const SKILL: &str = include_str!("../../plugin/skills/lodestone/SKILL.md");

/// The reminder's advice, ahead of what lodestone serves.
const ADVICE: &str = "To locate the code behind a described behaviour, search by what the code does before running `grep -r` or `rg` over the tree with guessed names. Before writing any new function, type or macro, when reviewing a diff that adds functions, and when consolidating duplicated code, look for existing code that already does it, and reuse what fits instead of writing a new version.";

/// The reminder's description of the tools, after what lodestone serves.
const TOOLS: &str = "The `semantic_code_search` tool finds code by what it does, which a text search cannot when you do not know the name, and the `similar_code_search` tool finds code that does the same job as given functions; pass it every function a diff adds or a refactor moves, in one call (see the lodestone skill). If they are listed only as deferred tools, load them first with ToolSearch (query `+lodestone search`). Use a text search (grep, rg) for exact strings.";

/// Alternatives in a tree search's pattern from which it reads as guessing at names.
const GUESSES: usize = 3;

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
            "{ADVICE} Lodestone indexes these repositories in this folder: {}. Pass the one you work in as `repo` (its name, or the path of any of its git worktrees) to lodestone's tools. {TOOLS}",
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
    /// Every subagent start: a subagent sees none of its parent session's reminders.
    SubagentStart,
    /// Each shell command; adds [`nudge`] to one that searches by guessed names.
    PreToolUse,
}

impl HookEvent {
    /// The harness's name for the event.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::PreToolUse => "PreToolUse",
        }
    }

    /// The `lode` subcommand that answers it.
    pub fn args(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "hook session-start",
            HookEvent::UserPromptSubmit => "hook prompt-submit",
            HookEvent::SubagentStart => "hook subagent-start",
            HookEvent::PreToolUse => "hook pre-tool-use",
        }
    }

    /// What the harness matches before running the hook, if it runs for only some events.
    pub fn matcher(self) -> Option<&'static str> {
        match self {
            HookEvent::SessionStart => Some("startup|resume|clear|compact"),
            HookEvent::PreToolUse => Some("Bash"),
            HookEvent::UserPromptSubmit | HookEvent::SubagentStart => None,
        }
    }
}

/// `context` as the hook output that adds it to the model's context.
fn output(event: HookEvent, context: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": event.name(),
            "additionalContext": context,
        }
    })
    .to_string()
}

/// The hook's output for `event` in session `session` (the harness's id) in `dir`: the reminder
/// as hook JSON when `dir` is in a registered worktree, otherwise `None`.
///
/// The session's own hooks keep its marker (see [`marker`]) current. `UserPromptSubmit` adds the
/// reminder only when it writes a session's first marker, so never without a session id;
/// `SessionStart` always adds it, since a compacted session has lost it. `SubagentStart` always
/// adds it too, and leaves the marker to the session. `PreToolUse` adds nothing here (see
/// [`nudge`]).
pub fn hook(
    event: HookEvent,
    dir: &Path,
    session: Option<&str>,
    config: &Config,
    config_path: Option<&Path>,
) -> Option<String> {
    let path = session
        .filter(|_| event != HookEvent::SubagentStart)
        .and_then(|id| marker::path(&config.data_dir(), id));
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
        HookEvent::SessionStart | HookEvent::SubagentStart => true,
        HookEvent::UserPromptSubmit => path.is_some() && previous.is_none(),
        HookEvent::PreToolUse => false,
    };
    let served = remind.then(|| serves(dir, config, config_path)).flatten()?;
    Some(output(event, &reminder(&served)))
}

/// The `PreToolUse` hook's output for shell `command` run in `dir`: a pointer to
/// `semantic_code_search` when the command searches a tree lodestone serves for [`GUESSES`] or
/// more alternative names, otherwise `None`. It decides nothing about the command.
pub fn nudge(
    command: &str,
    dir: &Path,
    config: &Config,
    config_path: Option<&Path>,
) -> Option<String> {
    let search = shell::analyse(command, Some(dir))
        .searches
        .into_iter()
        .filter(|s| s.tree && s.alternatives >= GUESSES)
        .max_by_key(|s| s.alternatives)?;
    let target = search.target.as_deref().unwrap_or(dir);
    let top = registry::checkout_top(target);
    let repo = match serves(target, config, config_path)? {
        // A search in the session's own checkout needs no `repo`; one in another, such as a
        // linked worktree inside it, does.
        Served::Worktree if top.is_some() && top == registry::checkout_top(dir) => String::new(),
        Served::Worktree => format!(
            " Pass `repo`: `{}`.",
            top.as_deref().unwrap_or(target).display()
        ),
        Served::Inside(repos) => format!(
            " Pass the repository as `repo`: {}.",
            registry::listing(&repos)
        ),
    };
    let context = format!(
        "This search tries {} names. lodestone's `semantic_code_search` finds code by what it does, without the name: describe the behaviour in `query` and give the guesses as `example_identifiers`.{repo} A text search suits exact strings.",
        search.alternatives
    );
    Some(output(HookEvent::PreToolUse, &context))
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
