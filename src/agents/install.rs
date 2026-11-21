//! `lode install` / `lode uninstall`: the skill, the MCP server and the reminder hooks for
//! Claude Code and Codex.
//!
//! The skill lives in the shared `.agents/skills/` location (read by Codex, and by Claude Code
//! at user level); Claude also gets a symlink from `.claude/skills/`, its documented location.
//! MCP servers and hooks have no shared location, so each harness gets its own. The MCP server is
//! `lode mcp`, which starts the daemon on demand.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

use super::{HookEvent, SKILL};

const NAME: &str = "lodestone";
const HOOK_TIMEOUT_SECS: u64 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    pub const ALL: [Harness; 2] = [Harness::Claude, Harness::Codex];

    /// The harness's CLI, which manages its user-level MCP servers.
    pub fn program(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }

    /// The hooks that add the reminder. Claude Code in the desktop app skips SessionStart hooks
    /// at startup, so Claude also gets the first-prompt hook; its subagents get their own, and
    /// its shell commands the nudge.
    fn hook_events(self) -> &'static [HookEvent] {
        match self {
            Harness::Claude => &[
                HookEvent::SessionStart,
                HookEvent::UserPromptSubmit,
                HookEvent::SubagentStart,
                HookEvent::PreToolUse,
            ],
            Harness::Codex => &[HookEvent::SessionStart],
        }
    }

    /// Its config directory name (`~/.claude`, `<repo>/.codex`).
    fn dir(self) -> &'static str {
        match self {
            Harness::Claude => ".claude",
            Harness::Codex => ".codex",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Scope {
    /// Every session of the user (`~/...`).
    User,
    /// One repository (`<repo>/...`), for committing alongside the code.
    Project,
}

/// Where and what to install.
pub struct Target {
    pub scope: Scope,
    pub home: PathBuf,
    /// Repository root, for `Scope::Project`.
    pub project: PathBuf,
    /// The `lode` binary user-level config runs.
    pub exe: PathBuf,
}

impl Target {
    fn base(&self) -> &Path {
        match self.scope {
            Scope::User => &self.home,
            Scope::Project => &self.project,
        }
    }

    /// Joined component by component: it is also a symlink's target, which Windows stores
    /// verbatim and resolves only with `\` separators.
    fn skill_dir(&self) -> PathBuf {
        self.base().join(".agents").join("skills").join(NAME)
    }

    fn claude_skill_link(&self) -> PathBuf {
        self.base().join(".claude/skills").join(NAME)
    }

    /// Relative in a repository so the link survives a clone.
    fn claude_skill_link_target(&self) -> PathBuf {
        match self.scope {
            Scope::User => self.skill_dir(),
            Scope::Project => Path::new("..")
                .join("..")
                .join(".agents")
                .join("skills")
                .join(NAME),
        }
    }

    fn codex_config(&self) -> PathBuf {
        self.base().join(Harness::Codex.dir()).join("config.toml")
    }

    fn hooks_file(&self, h: Harness) -> PathBuf {
        let file = match h {
            Harness::Claude => "settings.json",
            Harness::Codex => "hooks.json",
        };
        self.base().join(h.dir()).join(file)
    }

    /// `lode`: this binary for user config; from PATH in a repository, which other machines share.
    fn program(&self) -> String {
        match self.scope {
            Scope::User => self.exe.display().to_string(),
            Scope::Project => "lode".into(),
        }
    }

    fn hook_command(&self, event: HookEvent) -> String {
        let args = event.args();
        // A harness takes exit status 2 from this hook as "block the tool call", and that is
        // what a `lode` too old to know the subcommand exits with.
        let guard = if event == HookEvent::PreToolUse {
            " || true"
        } else {
            ""
        };
        match self.scope {
            Scope::User => format!("'{}' {args}{guard}", self.program()),
            Scope::Project => format!("{} {args}{guard}", self.program()),
        }
    }
}

/// One change to the user's or repository's files, or one harness CLI call.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Write {
        path: PathBuf,
        contents: String,
    },
    Link {
        link: PathBuf,
        target: PathBuf,
    },
    /// Removes the link to `target` at `link` that [`Step::Link`] made.
    Unlink {
        link: PathBuf,
        target: PathBuf,
    },
    /// Removes a skill file lodestone shipped, or a skill directory left empty.
    Remove {
        path: PathBuf,
    },
    Run {
        program: String,
        args: Vec<String>,
        may_fail: bool,
    },
    /// Adds (`add`) or removes our `event` hook, running `command`, in a Claude settings or
    /// Codex hooks JSON file.
    Hook {
        path: PathBuf,
        event: HookEvent,
        command: String,
        add: bool,
    },
    /// Adds or removes our server (`<program> mcp`) in a `.mcp.json` file.
    McpJson {
        path: PathBuf,
        program: Option<String>,
    },
    /// Adds or removes our `[mcp_servers.lodestone]` table in a Codex `config.toml`.
    CodexToml {
        path: PathBuf,
        program: Option<String>,
    },
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = |add: bool| if add { "add" } else { "remove" };
        match self {
            Step::Write { path, .. } => write!(f, "write {}", path.display()),
            Step::Link { link, target } => {
                write!(f, "link {} -> {}", link.display(), target.display())
            }
            Step::Unlink { link, .. } => write!(f, "unlink {}", link.display()),
            Step::Remove { path } => write!(f, "remove {}", path.display()),
            Step::Run { program, args, .. } => write!(f, "run {program} {}", args.join(" ")),
            Step::Hook {
                path, event, add, ..
            } => write!(
                f,
                "{} {} hook in {}",
                verb(*add),
                event.name(),
                path.display()
            ),
            Step::McpJson { path, program } | Step::CodexToml { path, program } => write!(
                f,
                "{} MCP server `{NAME}` in {}",
                verb(program.is_some()),
                path.display()
            ),
        }
    }
}

/// The MCP server is `lode mcp`, a stdio relay that starts the daemon on demand.
fn mcp_steps(h: Harness, t: &Target, add: bool) -> Vec<Step> {
    let program = t.program();
    let run = |args: &[&str], may_fail: bool| Step::Run {
        program: h.program().into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        may_fail,
    };
    match (h, t.scope) {
        (Harness::Claude, Scope::User) => {
            // Re-adding replaces an older entry (another binary path, or the old HTTP server).
            let mut steps = vec![run(&["mcp", "remove", "--scope", "user", NAME], true)];
            if add {
                steps.push(run(
                    &["mcp", "add", "--scope", "user", NAME, "--", &program, "mcp"],
                    false,
                ));
            }
            steps
        }
        (Harness::Claude, Scope::Project) => vec![Step::McpJson {
            path: t.project.join(".mcp.json"),
            program: add.then_some(program),
        }],
        // Written directly at both scopes: `codex mcp add` cannot set the approval mode.
        (Harness::Codex, _) => vec![Step::CodexToml {
            path: t.codex_config(),
            program: add.then_some(program),
        }],
    }
}

/// Steps that install lodestone into each harness.
pub fn install(harnesses: &[Harness], t: &Target) -> Vec<Step> {
    let mut steps = vec![Step::Write {
        path: t.skill_dir().join("SKILL.md"),
        contents: SKILL.into(),
    }];
    for &h in harnesses {
        if h == Harness::Claude {
            steps.push(Step::Link {
                link: t.claude_skill_link(),
                target: t.claude_skill_link_target(),
            });
        }
        steps.extend(mcp_steps(h, t, true));
        steps.extend(h.hook_events().iter().map(|&event| Step::Hook {
            path: t.hooks_file(h),
            event,
            command: t.hook_command(event),
            add: true,
        }));
    }
    steps
}

/// Steps that remove lodestone from each harness. The shared skill goes only when every
/// harness is uninstalled, since the others read it too.
pub fn uninstall(harnesses: &[Harness], t: &Target) -> Vec<Step> {
    let mut steps = Vec::new();
    for &h in harnesses {
        if h == Harness::Claude {
            steps.push(Step::Unlink {
                link: t.claude_skill_link(),
                target: t.claude_skill_link_target(),
            });
        }
        steps.extend(mcp_steps(h, t, false));
        steps.extend(h.hook_events().iter().map(|&event| Step::Hook {
            path: t.hooks_file(h),
            event,
            command: t.hook_command(event),
            add: false,
        }));
    }
    if Harness::ALL.iter().all(|h| harnesses.contains(h)) {
        // The directory goes only once empty: anything else in it is the user's.
        steps.push(Step::Remove {
            path: t.skill_dir().join("SKILL.md"),
        });
        steps.push(Step::Remove {
            path: t.skill_dir(),
        });
    }
    steps
}

/// Checks `steps` against the files as they are, before any is applied, so that a conflict
/// changes nothing: anything but lodestone's link where the skill link goes, a skill file
/// lodestone did not write, a directory holding other files, or a config file that does not
/// parse. [`apply`] checks each step again.
pub fn check(steps: &[Step]) -> anyhow::Result<()> {
    let mut removed: Vec<&Path> = Vec::new();
    for step in steps {
        check_step(step, &removed)?;
        if let Step::Remove { path } = step {
            removed.push(path);
        }
    }
    Ok(())
}

/// [`check`] for one step, given the paths earlier steps remove.
fn check_step(step: &Step, removed: &[&Path]) -> anyhow::Result<()> {
    match step {
        Step::Write { path, contents } => match fs::read_to_string(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(text) if text == *contents || is_shipped_skill(&text) => Ok(()),
            _ => bail!(
                "{} was not written by lodestone; move it aside and install again",
                path.display()
            ),
        },
        Step::Link { link, target } => match fs::symlink_metadata(link) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            _ if is_our_link(link, target) => Ok(()),
            _ => bail!(
                "{} already exists and is not lodestone's link; move it aside and install again",
                link.display()
            ),
        },
        Step::Unlink { link, target } => match fs::symlink_metadata(link) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            _ if is_our_link(link, target) => Ok(()),
            _ => bail!(
                "not removing {}: it is not the link lodestone made",
                link.display()
            ),
        },
        Step::Remove { path } => match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(m) if m.is_dir() => {
                for entry in fs::read_dir(path)? {
                    anyhow::ensure!(
                        removed.contains(&entry?.path().as_path()),
                        "not removing {}: it holds files lodestone did not write",
                        path.display()
                    );
                }
                Ok(())
            }
            Ok(m)
                if m.is_file() && fs::read_to_string(path).is_ok_and(|t| is_shipped_skill(&t)) =>
            {
                Ok(())
            }
            _ => bail!(
                "not removing {}: lodestone did not write it",
                path.display()
            ),
        },
        Step::Run { .. } => Ok(()),
        Step::Hook { .. } | Step::McpJson { .. } | Step::CodexToml { .. } => edit(step).map(|_| ()),
    }
}

/// Applies one step, after checking it (see [`check`]).
pub fn apply(step: &Step) -> anyhow::Result<()> {
    check_step(step, &[])?;
    match step {
        Step::Write { path, contents } => {
            create_parent(path)?;
            fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
        }
        Step::Link { link, target } => {
            create_parent(link)?;
            remove(link)?;
            symlink(target, link).with_context(|| format!("linking {}", link.display()))
        }
        Step::Unlink { link, .. } | Step::Remove { path: link } => remove(link),
        Step::Run {
            program,
            args,
            may_fail,
        } => {
            // Windows' own lookup tries only `.exe`.
            let out = Command::new(find_program(program).unwrap_or_else(|| program.into()))
                .args(args)
                .output()
                .with_context(|| {
                    format!(
                        "{program} not found; run it yourself: {program} {}",
                        args.join(" ")
                    )
                })?;
            if !out.status.success() && !may_fail {
                bail!(
                    "{program} {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        }
        Step::Hook { .. } | Step::McpJson { .. } | Step::CodexToml { .. } => {
            let (path, old, new) = edit(step)?;
            write_if_changed(path, &old, &new)
        }
    }
}

/// A config-editing step's file, its text and its text once edited.
fn edit(step: &Step) -> anyhow::Result<(&Path, String, String)> {
    let (Step::Hook { path, .. } | Step::McpJson { path, .. } | Step::CodexToml { path, .. }) =
        step
    else {
        bail!("`{step}` edits no config file");
    };
    let old = read_or_empty(path)?;
    let new = match step {
        Step::Hook {
            event,
            command,
            add,
            ..
        } => edit_json(&old, |doc| set_hook(doc, *event, command, *add)),
        Step::McpJson { program, .. } => {
            edit_json(&old, |doc| set_mcp_server(doc, program.as_deref()))
        }
        Step::CodexToml { program, .. } => set_codex_server(&old, program.as_deref()),
        _ => Ok(old.clone()),
    }
    .with_context(|| format!("editing {}; left unchanged", path.display()))?;
    Ok((path, old, new))
}

/// blake3 of each SKILL.md lodestone has shipped, with `\n` line endings: a skill file lodestone
/// may overwrite or remove. A test fails until the current one is listed.
const SHIPPED_SKILLS: [&str; 5] = [
    "f85f9c394917c983324f0c97ae91ce5f3749f0f54d611d61ada0bb88c77f8503",
    "1de52c75ad8593becdff37119fbd943d79e5c796cce1c9a282af8c82c36afab4",
    "c8f51de035ef5db53a6692ca3c5721065d6a76c43e5bbbaa7f5a5408edd2b903",
    "21831d5b2cc6f40c0ed81580cc088c0b3d916142ad2014145571166c0c1eb9db",
    "7f8217d8f5f2207e2cd0b73ca1ba8e390b69b91cf3d60061d92058ec41e36570",
];

/// Whether `text` is a SKILL.md lodestone shipped.
fn is_shipped_skill(text: &str) -> bool {
    let hash = blake3::hash(text.replace("\r\n", "\n").as_bytes());
    SHIPPED_SKILLS.contains(&hash.to_hex().as_str())
}

/// Whether `link` is the link lodestone makes: to `target`, as written or resolved.
fn is_our_link(link: &Path, target: &Path) -> bool {
    let Ok(found) = fs::read_link(link) else {
        return false;
    };
    let resolve = |t: &Path| {
        link.parent()
            .map(|d| d.join(t))
            .and_then(|p| p.canonicalize().ok())
    };
    found == target || resolve(&found).is_some_and(|f| Some(f) == resolve(target))
}

/// Executable extensions Windows tries when `PATHEXT` is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Where `program` is on `PATH`: the first file of that name this user may run. On Windows,
/// where a command is installed as `.exe` or `.cmd`, with each extension in `PATHEXT`.
pub fn find_program(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let pathext =
        cfg!(windows).then(|| std::env::var("PATHEXT").unwrap_or_else(|_| DEFAULT_PATHEXT.into()));
    let names = program_names(program, pathext.as_deref());
    std::env::split_paths(&path)
        .find_map(|dir| names.iter().map(|n| dir.join(n)).find(|p| is_executable(p)))
}

/// A file this process may execute (`access(2)` with `X_OK`, as the shell's lookup checks).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a NUL-terminated path that outlives the call.
    path.is_file() && unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } == 0
}

/// Windows runs any file whose extension is in `PATHEXT`.
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The file names `program` may have: itself, or with each of `pathext`'s extensions.
fn program_names(program: &str, pathext: Option<&str>) -> Vec<String> {
    match pathext {
        None => vec![program.to_string()],
        Some(exts) => exts
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| format!("{program}{e}"))
            .collect(),
    }
}

fn create_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(())
}

/// Removes a file, a link or an empty directory; `check_step` decides which are lodestone's.
fn remove(path: &Path) -> anyhow::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => Err(e),
        // Windows removes a directory symlink only as a directory.
        Ok(m) if m.is_symlink() => fs::remove_file(path).or_else(|_| fs::remove_dir(path)),
        Ok(m) if m.is_dir() => fs::remove_dir(path),
        Ok(_) => fs::remove_file(path),
    };
    result.with_context(|| format!("removing {}", path.display()))
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

fn read_or_empty(path: &Path) -> anyhow::Result<String> {
    match fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        r => r.with_context(|| format!("reading {}", path.display())),
    }
}

fn write_if_changed(path: &Path, old: &str, new: &str) -> anyhow::Result<()> {
    if old == new {
        return Ok(());
    }
    create_parent(path)?;
    fs::write(path, new).with_context(|| format!("writing {}", path.display()))
}

/// JSON object `text` (empty: a new object) edited by `edit`, keeping its key order.
fn edit_json(text: &str, edit: impl FnOnce(&mut Map<String, Value>)) -> anyhow::Result<String> {
    let mut doc: Map<String, Value> = if text.trim().is_empty() {
        Map::new()
    } else {
        serde_json::from_str(text).context("not a JSON object")?
    };
    edit(&mut doc);
    Ok(format!("{}\n", serde_json::to_string_pretty(&doc)?))
}

/// Whether `hook` is our `event` hook: `command`, which this install writes, or what an install
/// from another path wrote, `'<absolute path to lode>' <args>` (guarded for `PreToolUse`). Any
/// other command is the user's, even one that runs `lode`.
fn is_ours(hook: &Value, event: HookEvent, command: &str) -> bool {
    let Some(theirs) = hook["command"].as_str() else {
        return false;
    };
    if theirs == command {
        return true;
    }
    let theirs = match event {
        HookEvent::PreToolUse => theirs.strip_suffix(" || true"),
        _ => Some(theirs),
    };
    theirs
        .and_then(|c| c.strip_suffix(event.args()))
        .and_then(|p| p.strip_suffix("' "))
        .and_then(|p| p.strip_prefix('\''))
        .filter(|path| !path.contains('\'') && is_absolute(path))
        .and_then(|path| Path::new(path).file_name())
        .is_some_and(|name| name == "lode" || name == "lode.exe")
}

/// Absolute on this platform, or written as one for a Unix shell.
fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || Path::new(path).is_absolute()
}

/// Replaces our `event` hook with one running `command` (`add`), or removes it; other hooks stay.
fn set_hook(doc: &mut Map<String, Value>, event: HookEvent, command: &str, add: bool) {
    let hooks = doc
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    let Some(hooks) = hooks else { return };
    let mut groups: Vec<Value> = hooks
        .remove(event.name())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    for g in &mut groups {
        if let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) {
            list.retain(|h| !is_ours(h, event, command));
        }
    }
    groups.retain(|g| g["hooks"].as_array().is_none_or(|l| !l.is_empty()));
    if add {
        let mut group = Map::new();
        if let Some(matcher) = event.matcher() {
            group.insert("matcher".into(), matcher.into());
        }
        group.insert(
            "hooks".into(),
            json!([{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECS }]),
        );
        groups.push(Value::Object(group));
    }
    if !groups.is_empty() {
        hooks.insert(event.name().into(), Value::Array(groups));
    }
    if hooks.is_empty() {
        doc.remove("hooks");
    }
}

fn set_mcp_server(doc: &mut Map<String, Value>, program: Option<&str>) {
    let servers = doc
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    let Some(servers) = servers else { return };
    match program {
        Some(program) => {
            servers.insert(
                NAME.into(),
                json!({ "type": "stdio", "command": program, "args": ["mcp"] }),
            );
        }
        None => {
            servers.remove(NAME);
        }
    }
    if servers.is_empty() {
        doc.remove("mcpServers");
    }
}

/// Settings of Codex's streamable HTTP MCP servers, which it rejects for a stdio server (as of
/// Codex 0.171).
const HTTP_SETTINGS: [&str; 8] = [
    "url",
    "bearer_token",
    "bearer_token_env_var",
    "http_headers",
    "env_http_headers",
    "oauth_resource",
    "oauth",
    "auth",
];

/// Codex config `text` with our `[mcp_servers.lodestone]` server set to run `program`, or removed
/// (`None`). Edited as a TOML document, so the rest keeps its comments and layout, and settings
/// the user added to our table stay.
fn set_codex_server(text: &str, program: Option<&str>) -> anyhow::Result<String> {
    let mut doc: DocumentMut = text.parse().context("not valid TOML")?;
    match program {
        Some(program) => {
            let servers = doc.entry("mcp_servers").or_insert_with(|| {
                let mut t = Table::new();
                t.set_implicit(true);
                Item::Table(t)
            });
            let fresh = if servers.is_inline_table() {
                Item::Value(InlineTable::new().into())
            } else {
                Item::Table(Table::new())
            };
            let servers = servers
                .as_table_like_mut()
                .context("`mcp_servers` is not a table")?;
            if servers.get(NAME).is_none() {
                servers.insert(NAME, fresh);
            }
            let ours = servers
                .get_mut(NAME)
                .and_then(Item::as_table_like_mut)
                .with_context(|| format!("`mcp_servers.{NAME}` is not a table"))?;
            // A stdio server, also when replacing the old HTTP one: Codex rejects both kinds of
            // settings in one table. Settings of both kinds (timeouts, tool lists) stay.
            for http in HTTP_SETTINGS {
                ours.remove(http);
            }
            ours.insert("command", value(program));
            ours.insert("args", value(Array::from_iter(["mcp"])));
            // The tools only read the index (and say so), so Codex need not ask before each call.
            ours.insert("default_tools_approval_mode", value("approve"));
        }
        None => {
            if let Some(servers) = doc.get_mut("mcp_servers").and_then(Item::as_table_like_mut) {
                servers.remove(NAME);
                if servers.is_empty() {
                    doc.remove("mcp_servers");
                }
            }
        }
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tempdir;

    fn target(scope: Scope, root: &Path) -> Target {
        Target {
            scope,
            home: root.join("home"),
            project: root.join("repo"),
            exe: PathBuf::from("/opt/bin/lode"),
        }
    }

    fn run_all(steps: &[Step]) {
        for s in steps {
            // Harness CLIs are not under test; everything else is applied for real.
            if !matches!(s, Step::Run { .. }) {
                apply(s).unwrap();
            }
        }
    }

    fn read_json(p: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn user_install_shares_the_skill_and_runs_each_cli() {
        let root = tempdir();
        let t = target(Scope::User, &root);
        let home = root.join("home");
        // An entry from an older install (`codex mcp add`), next to the user's own settings.
        let codex = home.join(".codex/config.toml");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        fs::write(
            &codex,
            "model = \"x\"\n\n[mcp_servers.lodestone]\ncommand = \"/old/lode\"\nargs = [\"mcp\"]\n\n\
             [mcp_servers.other]\ncommand = \"o\"\n",
        )
        .unwrap();
        let steps = install(&Harness::ALL, &t);
        let runs: Vec<String> = steps
            .iter()
            .filter(|s| matches!(s, Step::Run { .. }))
            .map(Step::to_string)
            .collect();
        assert_eq!(
            runs,
            [
                "run claude mcp remove --scope user lodestone",
                "run claude mcp add --scope user lodestone -- /opt/bin/lode mcp",
            ]
        );
        run_all(&steps);
        assert_eq!(
            fs::read_to_string(&codex).unwrap(),
            "model = \"x\"\n\n[mcp_servers.lodestone]\ncommand = \"/opt/bin/lode\"\nargs = [\"mcp\"]\n\
             default_tools_approval_mode = \"approve\"\n\n[mcp_servers.other]\ncommand = \"o\"\n"
        );
        let skill = home.join(".agents/skills/lodestone/SKILL.md");
        assert_eq!(fs::read_to_string(&skill).unwrap(), SKILL);
        assert_eq!(
            fs::read_to_string(home.join(".claude/skills/lodestone/SKILL.md")).unwrap(),
            SKILL
        );
        for file in [".claude/settings.json", ".codex/hooks.json"] {
            let doc = read_json(&home.join(file));
            assert_eq!(
                doc["hooks"]["SessionStart"][0]["hooks"][0]["command"],
                "'/opt/bin/lode' hook session-start"
            );
        }
        // Claude Code in the desktop app skips SessionStart at startup; Codex does not.
        let prompt_hook =
            |file: &str| read_json(&home.join(file))["hooks"]["UserPromptSubmit"].clone();
        assert_eq!(
            prompt_hook(".claude/settings.json"),
            json!([{ "hooks": [{
                "type": "command",
                "command": "'/opt/bin/lode' hook prompt-submit",
                "timeout": HOOK_TIMEOUT_SECS,
            }] }])
        );
        assert_eq!(prompt_hook(".codex/hooks.json"), Value::Null);
        assert_eq!(
            read_json(&home.join(".claude/settings.json"))["hooks"]["SubagentStart"][0]["hooks"][0]
                ["command"],
            "'/opt/bin/lode' hook subagent-start"
        );
        assert_eq!(
            read_json(&home.join(".claude/settings.json"))["hooks"]["PreToolUse"],
            json!([{ "matcher": "Bash", "hooks": [{
                "type": "command",
                "command": "'/opt/bin/lode' hook pre-tool-use || true",
                "timeout": HOOK_TIMEOUT_SECS,
            }] }])
        );

        run_all(&uninstall(&[Harness::Claude], &t));
        assert!(skill.exists(), "Codex still reads the shared skill");
        assert!(fs::symlink_metadata(home.join(".claude/skills/lodestone")).is_err());
        assert_eq!(read_json(&home.join(".claude/settings.json")), json!({}));
        run_all(&uninstall(&[Harness::Codex, Harness::Claude], &t));
        assert!(!skill.exists());
        assert_eq!(
            fs::read_to_string(&codex).unwrap(),
            "model = \"x\"\n\n[mcp_servers.other]\ncommand = \"o\"\n"
        );
    }

    #[test]
    fn windows_programs_are_found_by_their_pathext_extensions() {
        assert_eq!(program_names("claude", None), ["claude"]);
        assert_eq!(
            program_names("claude", Some(DEFAULT_PATHEXT)),
            ["claude.COM", "claude.EXE", "claude.BAT", "claude.CMD"]
        );
    }

    /// Every file under `dir`, with its contents.
    fn snapshot(dir: &Path) -> Vec<(PathBuf, Option<String>)> {
        let mut out = vec![(dir.to_path_buf(), fs::read_to_string(dir).ok())];
        if fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
            for e in fs::read_dir(dir).unwrap() {
                out.extend(snapshot(&e.unwrap().path()));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn the_current_skill_is_a_shipped_one() {
        assert!(is_shipped_skill(SKILL));
        // Either line ending, whichever the checkout gave SKILL.
        let lf = SKILL.replace("\r\n", "\n");
        assert!(is_shipped_skill(&lf));
        assert!(is_shipped_skill(&lf.replace('\n', "\r\n")));
        assert!(!is_shipped_skill("---\nname: lodestone\n---\nmine\n"));
    }

    #[test]
    fn a_conflict_stops_install_before_anything_changes() {
        let root = tempdir();
        let t = target(Scope::Project, &root);
        let repo = root.join("repo");
        // The user's own skill, and a directory where the Claude link goes.
        let skill = repo.join(".agents/skills/lodestone/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "---\nname: lodestone\n---\nmine\n").unwrap();
        let before = snapshot(&repo);
        let err = check(&install(&Harness::ALL, &t)).unwrap_err();
        assert!(
            format!("{err:#}").contains("not written by lodestone"),
            "{err:#}"
        );
        for step in install(&Harness::ALL, &t) {
            if matches!(step, Step::Write { .. }) {
                assert!(apply(&step).is_err());
            }
        }
        assert_eq!(snapshot(&repo), before);

        fs::remove_file(&skill).unwrap();
        let own = repo.join(".claude/skills/lodestone/notes.md");
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        fs::write(&own, "mine").unwrap();
        let before = snapshot(&repo);
        let err = check(&install(&Harness::ALL, &t)).unwrap_err();
        assert!(
            format!("{err:#}").contains("is not lodestone's link"),
            "{err:#}"
        );
        assert_eq!(snapshot(&repo), before);
        // Nor does uninstall touch it.
        assert!(check(&uninstall(&Harness::ALL, &t)).is_err());
        assert!(
            apply(&Step::Remove {
                path: own.parent().unwrap().to_path_buf()
            })
            .is_err()
        );
        assert_eq!(snapshot(&repo), before);
    }

    #[test]
    fn uninstall_keeps_the_users_skill_files() {
        let root = tempdir();
        let t = target(Scope::Project, &root);
        let dir = root.join("repo/.agents/skills/lodestone");
        run_all(&install(&Harness::ALL, &t));
        let own = dir.join("notes.md");
        fs::write(&own, "mine").unwrap();
        let before = snapshot(&root.join("repo"));
        assert!(check(&uninstall(&Harness::ALL, &t)).is_err());
        assert_eq!(snapshot(&root.join("repo")), before);
        // A skill the user rewrote stays too.
        fs::remove_file(&own).unwrap();
        fs::write(dir.join("SKILL.md"), "mine").unwrap();
        assert!(check(&uninstall(&Harness::ALL, &t)).is_err());
        fs::write(dir.join("SKILL.md"), SKILL).unwrap();
        run_all(&uninstall(&Harness::ALL, &t));
        assert!(!dir.exists());
    }

    #[test]
    fn only_lodestones_link_is_replaced_or_removed() {
        let root = tempdir();
        let t = target(Scope::Project, &root);
        let repo = root.join("repo");
        let link = repo.join(".claude/skills/lodestone");
        // The user's own link, to their own skill: even one in a shared skill directory elsewhere.
        let theirs = root.join("dotfiles/.agents/skills/lodestone");
        fs::create_dir_all(&theirs).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&theirs, &link).unwrap();
        assert!(check(&install(&[Harness::Claude], &t)).is_err());
        assert!(check(&uninstall(&[Harness::Claude], &t)).is_err());
        assert_eq!(fs::read_link(&link).unwrap(), theirs);
        // An empty directory of theirs is no link either.
        fs::remove_file(&link)
            .or_else(|_| fs::remove_dir(&link))
            .unwrap();
        fs::create_dir(&link).unwrap();
        assert!(check(&uninstall(&[Harness::Claude], &t)).is_err());
        assert!(link.is_dir());
        // Lodestone's own link comes and goes.
        fs::remove_dir(&link).unwrap();
        run_all(&install(&[Harness::Claude], &t));
        run_all(&install(&[Harness::Claude], &t));
        run_all(&uninstall(&[Harness::Claude], &t));
        assert!(fs::symlink_metadata(&link).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn only_executable_files_are_programs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let file = dir.join("claude");
        fs::write(&file, "").unwrap();
        assert!(!is_executable(&file));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&file));
        assert!(!is_executable(&dir));
        // Runnable by others, not by its owner (this user).
        fs::set_permissions(&file, fs::Permissions::from_mode(0o601)).unwrap();
        assert!(!is_executable(&file));
    }

    #[test]
    fn codex_config_is_edited_as_toml() {
        let program = Some("lode");
        // A comment after the header, and a setting of the user's in our table.
        let text =
            "[mcp_servers.lodestone] # code search\ncommand = \"old\"\nstartup_timeout_sec = 30\n";
        let new = set_codex_server(text, program).unwrap();
        assert_eq!(
            new,
            "[mcp_servers.lodestone] # code search\ncommand = \"lode\"\nstartup_timeout_sec = 30\n\
             args = [\"mcp\"]\ndefault_tools_approval_mode = \"approve\"\n"
        );
        assert_eq!(set_codex_server(&new, program).unwrap(), new);
        // Servers as an inline table, and as dotted keys.
        let inline =
            set_codex_server("mcp_servers = { other = { command = \"o\" } }\n", program).unwrap();
        let parsed: toml::Table = toml::from_str(&inline).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["lodestone"]["command"].as_str(),
            Some("lode")
        );
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"].as_str(),
            Some("o")
        );
        let dotted = set_codex_server("mcp_servers.other.command = \"o\"\n", program).unwrap();
        let parsed: toml::Table = toml::from_str(&dotted).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["lodestone"]["args"][0].as_str(),
            Some("mcp")
        );
        assert_eq!(
            set_codex_server(&dotted, None).unwrap(),
            "mcp_servers.other.command = \"o\"\n"
        );
        // Replacing the old HTTP server keeps the settings stdio servers share.
        let http = "[mcp_servers.lodestone]\nurl = \"http://127.0.0.1:4600/mcp\"\n\
                    bearer_token_env_var = \"T\"\noauth_resource = \"http://r\"\n\
                    tool_timeout_sec = 60\n";
        assert_eq!(
            set_codex_server(http, program).unwrap(),
            "[mcp_servers.lodestone]\ntool_timeout_sec = 60\ncommand = \"lode\"\n\
             args = [\"mcp\"]\ndefault_tools_approval_mode = \"approve\"\n"
        );
        // Invalid TOML is reported, not rewritten.
        assert!(set_codex_server("[mcp_servers\n", program).is_err());
    }

    #[test]
    fn project_install_writes_committable_files() {
        let root = tempdir();
        let t = target(Scope::Project, &root);
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".codex")).unwrap();
        fs::write(
            repo.join(".codex/config.toml"),
            "# mine\nmodel = \"x\"\n\n[mcp_servers.other]\nurl = \"http://o\"\n",
        )
        .unwrap();
        let steps = install(&Harness::ALL, &t);
        assert!(!steps.iter().any(|s| matches!(s, Step::Run { .. })));
        run_all(&steps);
        assert_eq!(
            fs::read_link(repo.join(".claude/skills/lodestone")).unwrap(),
            Path::new("../../.agents/skills/lodestone")
        );
        assert!(repo.join(".claude/skills/lodestone/SKILL.md").exists());
        assert_eq!(
            read_json(&repo.join(".mcp.json")),
            json!({ "mcpServers": { "lodestone": { "type": "stdio", "command": "lode", "args": ["mcp"] } } })
        );
        assert_eq!(
            fs::read_to_string(repo.join(".codex/config.toml")).unwrap(),
            "# mine\nmodel = \"x\"\n\n[mcp_servers.other]\nurl = \"http://o\"\n\n\
             [mcp_servers.lodestone]\ncommand = \"lode\"\nargs = [\"mcp\"]\n\
             default_tools_approval_mode = \"approve\"\n"
        );
        assert_eq!(
            read_json(&repo.join(".codex/hooks.json"))["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "lode hook session-start"
        );
        // Installing twice changes nothing.
        let before = fs::read_to_string(repo.join(".codex/config.toml")).unwrap();
        run_all(&install(&Harness::ALL, &t));
        assert_eq!(
            fs::read_to_string(repo.join(".codex/config.toml")).unwrap(),
            before
        );

        run_all(&uninstall(&Harness::ALL, &t));
        assert_eq!(
            fs::read_to_string(repo.join(".codex/config.toml")).unwrap(),
            "# mine\nmodel = \"x\"\n\n[mcp_servers.other]\nurl = \"http://o\"\n"
        );
        assert_eq!(read_json(&repo.join(".mcp.json")), json!({}));
        assert!(!repo.join(".agents/skills/lodestone").exists());
    }

    #[test]
    fn set_hook_keeps_other_hooks_and_key_order() {
        let mut doc: Map<String, Value> = serde_json::from_value(json!({
            "model": "opus",
            "hooks": {
                "SessionStart": [
                    { "hooks": [
                        { "type": "command", "command": "other" },
                        { "type": "command", "command": "'/old/lode' hook session-start" },
                        { "type": "command", "command": "lode hook session-start && ./team-setup" },
                        { "type": "command", "command": "./setup; lode hook session-start" },
                        { "type": "command", "command": "./tools/lode.sh hook session-start" },
                        { "type": "command", "command": "./bootstrap;/opt/bin/lode hook session-start" }
                    ] }
                ],
                "Stop": [ { "hooks": [ { "type": "command", "command": "bye" } ] } ]
            },
            "theme": "dark"
        }))
        .unwrap();
        set_hook(
            &mut doc,
            HookEvent::SessionStart,
            "lode hook session-start",
            true,
        );
        let keys: Vec<&str> = doc.keys().map(String::as_str).collect();
        assert_eq!(keys, ["model", "hooks", "theme"]);
        let groups = doc["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        // Only the command lodestone wrote is replaced; commands that also run lode stay.
        let theirs = json!([
            { "type": "command", "command": "other" },
            { "type": "command", "command": "lode hook session-start && ./team-setup" },
            { "type": "command", "command": "./setup; lode hook session-start" },
            { "type": "command", "command": "./tools/lode.sh hook session-start" },
            { "type": "command", "command": "./bootstrap;/opt/bin/lode hook session-start" }
        ]);
        assert_eq!(groups[0]["hooks"], theirs);
        assert_eq!(groups[1]["hooks"][0]["command"], "lode hook session-start");
        set_hook(
            &mut doc,
            HookEvent::SessionStart,
            "lode hook session-start",
            false,
        );
        let groups = doc["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"], theirs);
        assert!(doc["hooks"]["Stop"].is_array());
    }

    #[test]
    fn a_renamed_binary_replaces_and_removes_its_own_hook() {
        let theirs = json!({ "type": "command", "command": "'./tools/lode' hook session-start" });
        let mut doc: Map<String, Value> = serde_json::from_value(
            json!({ "hooks": { "SessionStart": [ { "hooks": [theirs] } ] } }),
        )
        .unwrap();
        let dev = "'/opt/bin/lode-dev' hook session-start";
        set_hook(&mut doc, HookEvent::SessionStart, dev, true);
        set_hook(&mut doc, HookEvent::SessionStart, dev, true);
        let commands = |doc: &Map<String, Value>| -> Vec<String> {
            doc["hooks"]["SessionStart"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["hooks"].as_array().unwrap().clone())
                .map(|h| h["command"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(commands(&doc), ["'./tools/lode' hook session-start", dev]);
        set_hook(&mut doc, HookEvent::SessionStart, dev, false);
        assert_eq!(commands(&doc), ["'./tools/lode' hook session-start"]);
    }

    #[test]
    fn hooks_are_ours_only_in_the_form_installed() {
        let pre = HookEvent::PreToolUse;
        let start = HookEvent::SessionStart;
        let ours = |c: &str, e, installed: &str| is_ours(&json!({ "command": c }), e, installed);
        // What this install writes, whatever the binary is called.
        let dev = "'/opt/bin/lode-dev' hook session-start";
        assert!(ours(dev, start, dev));
        assert!(ours(
            "lode hook pre-tool-use || true",
            pre,
            "lode hook pre-tool-use || true"
        ));
        // What an install from another path wrote.
        let installed = "'/new/lode' hook pre-tool-use || true";
        assert!(ours(
            "'/opt/my bin/lode' hook pre-tool-use || true",
            pre,
            installed
        ));
        let exe = std::env::current_dir().unwrap().join("lode.exe");
        let exe = format!("'{}' hook session-start", exe.display());
        assert!(ours(&exe, start, dev));
        assert!(!ours("'/old/lode' hook session-start || true", start, dev));
        assert!(!ours("'/old/lode' hook pre-tool-use", start, dev));
        // Wrappers, relative paths and compound commands that run `lode` are the user's.
        for theirs in [
            "./tools/lode.sh hook session-start",
            "'./tools/lode' hook session-start",
            "'./tools/lode.sh' hook session-start",
            "./bootstrap;/opt/bin/lode hook session-start",
            "/opt/bin/lode hook session-start",
            "lode hook session-start && ./more",
            "'/opt/bin/lode' hook session-start && ./more",
        ] {
            assert!(!ours(theirs, start, dev), "{theirs}");
        }
    }
}
