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
            steps.push(Step::Remove {
                path: t.claude_skill_link(),
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
        steps.push(Step::Remove {
            path: t.skill_dir(),
        });
    }
    steps
}

/// Applies one step.
pub fn apply(step: &Step) -> anyhow::Result<()> {
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
        Step::Remove { path } => remove(path),
        Step::Run {
            program,
            args,
            may_fail,
        } => {
            let out = Command::new(program).args(args).output().with_context(|| {
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
        Step::Hook {
            path,
            event,
            command,
            add,
        } => edit_json(path, |doc| set_hook(doc, *event, command, *add)),
        Step::McpJson { path, program } => {
            edit_json(path, |doc| set_mcp_server(doc, program.as_deref()))
        }
        Step::CodexToml { path, program } => {
            let text = read_or_empty(path)?;
            // The tools only read the index (and say so), so Codex need not ask before each call.
            let table = program.as_ref().map(|p| {
                format!(
                    "command = {}\nargs = [\"mcp\"]\ndefault_tools_approval_mode = \"approve\"\n",
                    Value::from(p.as_str())
                )
            });
            write_if_changed(
                path,
                &text,
                &set_toml_table(&text, MCP_TABLE, table.as_deref()),
            )
        }
    }
}

fn create_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(())
}

fn remove(path: &Path) -> anyhow::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        // Windows removes a directory symlink only as a directory.
        Ok(m) if m.is_symlink() => fs::remove_file(path).or_else(|_| fs::remove_dir(path)),
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

/// Edits a JSON object file in place, keeping its key order; a missing file starts empty.
fn edit_json(path: &Path, edit: impl FnOnce(&mut Map<String, Value>)) -> anyhow::Result<()> {
    let text = read_or_empty(path)?;
    let mut doc: Map<String, Value> = if text.trim().is_empty() {
        Map::new()
    } else {
        serde_json::from_str(&text)
            .with_context(|| format!("{} is not a JSON object; left unchanged", path.display()))?
    };
    edit(&mut doc);
    let new = format!("{}\n", serde_json::to_string_pretty(&doc)?);
    write_if_changed(path, &text, &new)
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

const MCP_TABLE: &str = "[mcp_servers.lodestone]";

/// Replaces (or removes, with `body` `None`) the TOML table headed `header`, as text, so the
/// rest of the file keeps its comments and layout.
fn set_toml_table(text: &str, header: &str, body: Option<&str>) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == header;
        }
        if !inside {
            out.push(line);
        }
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    let mut new = out.join("\n");
    if let Some(body) = body {
        if !new.is_empty() {
            new.push_str("\n\n");
        }
        new.push_str(header);
        new.push('\n');
        new.push_str(body);
    } else if !new.is_empty() {
        new.push('\n');
    }
    new
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
            "model = \"x\"\n\n[mcp_servers.other]\ncommand = \"o\"\n\n[mcp_servers.lodestone]\n\
             command = \"/opt/bin/lode\"\nargs = [\"mcp\"]\ndefault_tools_approval_mode = \"approve\"\n"
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
