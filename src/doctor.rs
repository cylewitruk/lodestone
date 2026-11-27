//! `lode doctor`: independent checks of the whole setup, each `ok`, `warn`, `fail` or `skip`, with
//! the fix for anything not ok. Reads files and asks a running daemon; starts nothing, embeds
//! nothing, and goes online only as the new-version notice does (and to reach a configured remote
//! embedder).

use std::any::Any;
use std::collections::BTreeSet;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::time::Duration;

use clap::ValueEnum;
use serde::Serialize;

use crate::agents::{self, Harness, Scope, State, Target};
use crate::api::types::StatusReport;
use crate::client::{self, Client};
use crate::config::{Backend, Config, Endpoint};
use crate::embed;
use crate::index::{self, Manifest};
use crate::registry::{self, Engine, Placement};
use crate::text;
use crate::update;
use crate::watch;

/// How long a configured remote embedder may take to answer.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(3);
/// Linux's per-user inotify watch limit.
const INOTIFY_LIMIT: &str = "/proc/sys/fs/inotify/max_user_watches";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Warn,
    Fail,
    /// Not applicable here, e.g. a harness that is not installed.
    Skip,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Warn => "warn",
            Outcome::Fail => "fail",
            Outcome::Skip => "skip",
        }
    }
}

/// What a check found, and for anything not ok, how to fix it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    pub outcome: Outcome,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Finding {
    pub fn ok(detail: impl Into<String>) -> Self {
        Self::new(Outcome::Ok, detail, None)
    }

    pub fn skip(detail: impl Into<String>) -> Self {
        Self::new(Outcome::Skip, detail, None)
    }

    pub fn warn(detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(Outcome::Warn, detail, Some(fix.into()))
    }

    pub fn fail(detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(Outcome::Fail, detail, Some(fix.into()))
    }

    fn new(outcome: Outcome, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            outcome,
            detail: detail.into(),
            fix,
        }
    }
}

/// One named check.
pub struct Check<'a> {
    pub name: String,
    run: Box<dyn Fn() -> anyhow::Result<Finding> + 'a>,
}

impl<'a> Check<'a> {
    pub fn new(name: impl Into<String>, run: impl Fn() -> anyhow::Result<Finding> + 'a) -> Self {
        Self {
            name: name.into(),
            run: Box::new(run),
        }
    }

    /// Runs the check. An error or a panic fails it, and stops no other check.
    pub fn run(&self) -> Report {
        let finding = match panic::catch_unwind(AssertUnwindSafe(|| (self.run)())) {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => Finding::new(Outcome::Fail, format!("{e:#}"), None),
            Err(p) => Finding::new(
                Outcome::Fail,
                format!("the check panicked: {}", panic_message(p.as_ref())),
                None,
            ),
        };
        Report {
            name: self.name.clone(),
            finding,
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown cause")
}

/// A check's name and finding.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub name: String,
    #[serde(flatten)]
    pub finding: Finding,
}

/// What `lode doctor` prints: every check's report, in order.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Doctor {
    pub checks: Vec<Report>,
}

impl Doctor {
    pub fn run(checks: &[Check]) -> Self {
        Self {
            checks: checks.iter().map(Check::run).collect(),
        }
    }

    /// How many checks failed.
    pub fn failed(&self) -> usize {
        self.checks
            .iter()
            .filter(|r| r.finding.outcome == Outcome::Fail)
            .count()
    }

    /// One line per check: outcome, name, what it found and the fix.
    pub fn to_text(&self) -> String {
        let width = self
            .checks
            .iter()
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(0);
        self.checks
            .iter()
            .map(|r| {
                let f = &r.finding;
                let fix = f
                    .fix
                    .as_ref()
                    .map(|fix| format!("; fix: {fix}"))
                    .unwrap_or_default();
                format!(
                    "{:<4}  {:<width$}  {}{fix}",
                    f.outcome.label(),
                    r.name,
                    f.detail
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What the checks look at.
pub struct Setup {
    pub config: Config,
    pub config_path: Option<PathBuf>,
    /// The directory whose repository is checked.
    pub cwd: PathBuf,
    /// The home directory harnesses keep their user config in.
    pub home: PathBuf,
    /// The `lode` binary installed config should run.
    pub exe: PathBuf,
    /// The running daemon, if one answers.
    pub daemon: Option<Client>,
}

/// Every check, in the order `lode doctor` prints them.
pub fn checks(s: &Setup) -> Vec<Check<'_>> {
    let mut checks = vec![
        Check::new("daemon", || daemon(s)),
        Check::new("model", || model(s)),
        Check::new("repository", || repository(s)),
    ];
    checks.extend(
        Harness::ALL
            .into_iter()
            .map(|h| Check::new(client_name(h), move || harness(s, h))),
    );
    checks.push(Check::new("watch limit", || watch_limit(s)));
    checks.push(Check::new("version", || version(s)));
    checks
}

/// The daemon: answering, at this binary's version, on the configured endpoints.
fn daemon(s: &Setup) -> anyhow::Result<Finding> {
    let at = s.config.endpoint();
    let Some(c) = &s.daemon else {
        return Ok(if client::daemon_present(&s.config) {
            Finding::warn(
                format!(
                    "a daemon owns {at} but does not answer: still loading its model, or stuck"
                ),
                format!(
                    "wait, or see {} and run `lode stop`",
                    s.config.data_dir().join("daemon.log").display()
                ),
            )
        } else {
            Finding::ok(format!(
                "not running; agent sessions start it on demand at {at}"
            ))
        });
    };
    let this = update::current().to_string();
    let restart = "`lode stop`; the next command starts this version";
    let Some(version) = c.version()? else {
        return Ok(Finding::fail(
            format!("the daemon at {at} runs an older lode than this one ({this})"),
            restart,
        ));
    };
    if version != this {
        return Ok(Finding::fail(
            format!("the daemon at {at} runs lode {version}, this is lode {this}"),
            restart,
        ));
    }
    if let Some(port) = s.config.tcp_port()
        && Endpoint::Tcp(port) != at
        && Client::connect(&Endpoint::Tcp(port)).is_none()
    {
        return Ok(Finding::warn(
            format!("lode {version} answers at {at}, but not on the configured port {port}"),
            "`lode stop`; the next start listens on it",
        ));
    }
    Ok(Finding::ok(format!("lode {version} at {at}")))
}

/// The embedding model: loaded by the daemon, or downloaded, or a remote embedder that answers.
fn model(s: &Setup) -> anyhow::Result<Finding> {
    let config = &s.config;
    let name = format!("{} on {}", config.model, config.backend);
    if s.daemon.is_some() {
        return Ok(Finding::ok(format!("{name}, loaded by the daemon")));
    }
    if config.model_config(&config.model).is_err() {
        return Ok(Finding::fail(
            format!("unknown model {}", config.model),
            "set `model` in config.toml to one `lode models` lists",
        ));
    }
    let backends = embed::available_backends();
    if !backends.contains(&config.backend) {
        let names: Vec<String> = backends.iter().map(Backend::to_string).collect();
        return Ok(Finding::fail(
            format!("this lode was built without the {} backend", config.backend),
            format!(
                "set `backend` in config.toml to one of {}",
                names.join(", ")
            ),
        ));
    }
    if config.backend == Backend::Remote {
        let url = &config.remote.url;
        let answers = ureq::get(url)
            .config()
            .timeout_global(Some(REMOTE_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .call()
            .is_ok();
        return Ok(if answers {
            Finding::ok(format!("{name}: {url} answers"))
        } else {
            Finding::fail(
                format!("{name}: {url} does not answer"),
                "start the embedding server, or set `remote.url` in config.toml",
            )
        });
    }
    let missing = embed::missing_files(config, &config.model, config.backend)?;
    Ok(if missing.is_empty() {
        Finding::ok(format!("{name}, downloaded"))
    } else {
        Finding::warn(
            format!("{name}: not downloaded yet ({})", missing.join(", ")),
            "the daemon downloads it when it starts: run `lode register` in a repository, or \
             start an agent session",
        )
    })
}

/// The current directory's repository: registered, indexed, fresh, and watched without errors.
fn repository(s: &Setup) -> anyhow::Result<Finding> {
    let root = match registry::place(&registry::registered_roots(&s.config), &s.cwd) {
        Placement::Registered(root) => root,
        Placement::NewWorktree(top) => {
            return Ok(Finding::ok(format!(
                "{} is a new worktree of a registered repository; its first agent session \
                 registers it",
                top.display()
            )));
        }
        Placement::Unregistered(top) => {
            return Ok(Finding::warn(
                format!("{} is not registered", top.display()),
                "`lode register`",
            ));
        }
        Placement::Outside => {
            return Ok(Finding::skip(format!(
                "{} is not in a git checkout",
                s.cwd.display()
            )));
        }
    };
    let repo = root.to_string_lossy();
    let status = match &s.daemon {
        Some(c) => c.status(Some(&repo))?,
        None => Engine::from_env(s.config.clone())?.status(Some(&repo))?,
    };
    Ok(index_finding(&status, text::now()))
}

/// What a registered worktree's status says of its index and watcher, as of `now`.
fn index_finding(st: &StatusReport, now: u64) -> Finding {
    let name = &st.name;
    let index = if st.indexing.running {
        Finding::ok(format!(
            "{name}: indexing, {}/{} files",
            text::count(st.indexing.files_done),
            text::count(st.indexing.files_total)
        ))
    } else if st.last_update == 0 {
        Finding::warn(format!("{name}: not indexed yet"), "`lode index`")
    } else if st.stale {
        Finding::warn(
            format!("{name}: stale ({})", st.stale_reasons.join("; ")),
            "`lode index`",
        )
    } else {
        Finding::ok(format!(
            "{name}: fresh, {} files, updated {}",
            text::count(st.files_indexed),
            text::ago(st.last_update, now)
        ))
    };
    if !watch::failed(&st.watcher) {
        return index;
    }
    let outcome = match index.outcome {
        Outcome::Ok => Outcome::Warn,
        o => o,
    };
    Finding::new(
        outcome,
        format!("{}; watcher {}", index.detail, st.watcher),
        index
            .fix
            .or_else(|| Some("`lode stop`; the next start watches it again".into())),
    )
}

/// `h`'s name for `--client`.
fn client_name(h: Harness) -> String {
    h.to_possible_value()
        .map(|v| v.get_name().to_string())
        .unwrap_or_default()
}

/// lodestone's MCP server, hooks and skill in harness `h`, if its CLI is on PATH: at user scope,
/// or at the current repository's when only it has lodestone.
fn harness(s: &Setup, h: Harness) -> anyhow::Result<Finding> {
    if agents::find_program(h.program()).is_none() {
        return Ok(Finding::skip(format!("`{}` is not on PATH", h.program())));
    }
    let target = |scope, project| Target {
        scope,
        home: s.home.clone(),
        project,
        exe: s.exe.clone(),
    };
    let user = target(Scope::User, s.home.clone());
    let project = registry::checkout_top(&s.cwd).map(|top| target(Scope::Project, top));
    let t = match project {
        _ if agents::has_lodestone(h, &user) => user,
        Some(p) if agents::has_lodestone(h, &p) => p,
        _ => {
            return Ok(Finding::warn(
                "lodestone is not installed",
                install_fix(h, Scope::User),
            ));
        }
    };
    Ok(install_finding(h, t.scope, &agents::installed(h, &t)))
}

/// The command that installs lodestone into `h` at `scope`.
fn install_fix(h: Harness, scope: Scope) -> String {
    let flag = match scope {
        Scope::User => "",
        Scope::Project => " --scope project",
    };
    format!("`lode install --client {}{flag}`", client_name(h))
}

/// What the states of the pieces of lodestone's install in `h` at `scope` add up to.
fn install_finding(h: Harness, scope: Scope, pieces: &[(String, State)]) -> Finding {
    let fix = install_fix(h, scope);
    let scope = match scope {
        Scope::User => "user",
        Scope::Project => "project",
    };
    let with = |state: State| -> Vec<&str> {
        pieces
            .iter()
            .filter(|(_, st)| *st == state)
            .map(|(p, _)| p.as_str())
            .collect()
    };
    let (current, outdated, missing) = (
        with(State::Current),
        with(State::Outdated),
        with(State::Missing),
    );
    if outdated.is_empty() && missing.is_empty() {
        return Finding::ok(format!("{scope} scope: {}", current.join(", ")));
    }
    let parts: Vec<String> = [("outdated", outdated), ("missing", missing)]
        .into_iter()
        .filter(|(_, p)| !p.is_empty())
        .map(|(what, p)| format!("{what}: {}", p.join(", ")))
        .collect();
    Finding::fail(format!("{scope} scope: {}", parts.join("; ")), fix)
}

/// Linux's inotify limit against the directories registered repositories have to watch (one
/// watch each), as their manifests list them.
fn watch_limit(s: &Setup) -> anyhow::Result<Finding> {
    let Ok(limit) = fs::read_to_string(INOTIFY_LIMIT) else {
        return Ok(Finding::skip("no per-user watch limit on this platform"));
    };
    let limit: usize = limit.trim().parse()?;
    let dirs: usize = registry::registered_roots(&s.config)
        .iter()
        .filter_map(|root| {
            Manifest::load(&index::index_dir(&s.config, root).join("manifest.json"))
                .ok()
                .flatten()
        })
        .map(|m| directories(&m))
        .sum();
    Ok(limit_finding(limit, dirs))
}

/// The directories holding a manifest's files, the root included.
fn directories(m: &Manifest) -> usize {
    let mut dirs = BTreeSet::from([""]);
    for file in m.files.keys() {
        let mut path = file.as_str();
        while let Some((parent, _)) = path.rsplit_once('/') {
            if !dirs.insert(parent) {
                break;
            }
            path = parent;
        }
    }
    dirs.len()
}

fn limit_finding(limit: usize, dirs: usize) -> Finding {
    let detail = format!(
        "{} directories to watch in registered repositories, limit {}",
        text::count(dirs),
        text::count(limit)
    );
    if dirs < limit {
        Finding::ok(detail)
    } else {
        Finding::warn(
            detail,
            format!("`sudo sysctl fs.inotify.max_user_watches={}`", dirs * 2),
        )
    }
}

/// A newer release, as the new-version notice knows it.
fn version(s: &Setup) -> anyhow::Result<Finding> {
    if update::opted_out() {
        return Ok(Finding::skip(format!(
            "release lookups are off ({})",
            update::OPT_OUT_ENV
        )));
    }
    let this = update::current();
    Ok(
        match update::latest_known(&s.config, s.config_path.as_deref()) {
            Some(latest) if latest > this => Finding::warn(
                format!("lode {latest} is available (this is {this})"),
                "`lode upgrade`",
            ),
            Some(_) => Finding::ok(format!("lode {this} is the latest release")),
            None => Finding::ok(format!(
                "lode {this}; the latest release is not looked up yet"
            )),
        },
    )
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::index::{FileEntry, Progress};

    fn status() -> StatusReport {
        StatusReport {
            name: "repo".into(),
            files_indexed: 1_200,
            last_update: 100,
            watcher: "watching (fsevents/inotify, 500ms debounce)".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_failing_or_panicking_check_stops_no_other() {
        let checks = [
            Check::new("error", || anyhow::bail!("broken")),
            Check::new("panic", || panic!("boom")),
            Check::new("fine", || Ok(Finding::ok("all good"))),
        ];
        let doctor = Doctor::run(&checks);
        assert_eq!(doctor.failed(), 2);
        assert_eq!(doctor.checks[0].finding.detail, "broken");
        assert_eq!(doctor.checks[1].finding.detail, "the check panicked: boom");
        assert_eq!(doctor.checks[2].finding, Finding::ok("all good"));
    }

    #[test]
    fn text_has_one_line_per_check_with_its_fix() {
        let doctor = Doctor {
            checks: vec![
                Report {
                    name: "daemon".into(),
                    finding: Finding::ok("lode 1.0.0"),
                },
                Report {
                    name: "repository".into(),
                    finding: Finding::warn("x is not registered", "`lode register`"),
                },
            ],
        };
        assert_eq!(
            doctor.to_text(),
            "ok    daemon      lode 1.0.0\n\
             warn  repository  x is not registered; fix: `lode register`"
        );
        let json = serde_json::to_value(&doctor).unwrap();
        assert_eq!(json["checks"][0]["outcome"], "ok");
        assert_eq!(json["checks"][0].get("fix"), None);
        assert_eq!(json["checks"][1]["fix"], "`lode register`");
    }

    #[test]
    fn index_findings() {
        let fresh = index_finding(&status(), 160);
        assert_eq!(
            fresh,
            Finding::ok("repo: fresh, 1,200 files, updated 1m ago")
        );

        let never = StatusReport {
            last_update: 0,
            ..status()
        };
        assert_matches!(
            index_finding(&never, 160),
            Finding { outcome: Outcome::Warn, ref fix, .. } if fix.as_deref() == Some("`lode index`")
        );

        let stale = StatusReport {
            stale: true,
            stale_reasons: vec!["3 files changed since last index".into()],
            ..status()
        };
        assert_eq!(
            index_finding(&stale, 160).detail,
            "repo: stale (3 files changed since last index)"
        );

        let indexing = StatusReport {
            stale: true,
            indexing: Progress {
                running: true,
                files_done: 5,
                files_total: 10,
                ..Default::default()
            },
            ..status()
        };
        assert_eq!(indexing.state(), "indexing");
        assert_eq!(index_finding(&indexing, 160).outcome, Outcome::Ok);
    }

    #[test]
    fn a_failed_watcher_warns() {
        let degraded = StatusReport {
            watcher: "polling every 60s (watcher failed: too many files)".into(),
            ..status()
        };
        let f = index_finding(&degraded, 160);
        assert_eq!(f.outcome, Outcome::Warn);
        assert!(
            f.detail
                .ends_with("watcher polling every 60s (watcher failed: too many files)")
        );
        assert!(f.fix.unwrap().contains("lode stop"));
    }

    #[test]
    fn install_findings() {
        let pieces = |states: &[State]| -> Vec<(String, State)> {
            states
                .iter()
                .enumerate()
                .map(|(i, s)| (format!("piece {i}"), *s))
                .collect()
        };
        let all = install_finding(
            Harness::Codex,
            Scope::User,
            &pieces(&[State::Current, State::Current]),
        );
        assert_eq!(all, Finding::ok("user scope: piece 0, piece 1"));

        let partial = install_finding(
            Harness::Antigravity,
            Scope::Project,
            &pieces(&[State::Current, State::Outdated, State::Missing]),
        );
        assert_eq!(
            partial,
            Finding::fail(
                "project scope: outdated: piece 1; missing: piece 2",
                "`lode install --client antigravity --scope project`"
            )
        );
    }

    #[test]
    fn directories_counts_each_once() {
        let mut m = Manifest::default();
        for f in ["a.rs", "src/b.rs", "src/c.rs", "src/x/d.rs", "tests/e.rs"] {
            m.files.insert(f.into(), FileEntry::default());
        }
        // "", src, src/x, tests
        assert_eq!(directories(&m), 4);
        assert_eq!(directories(&Manifest::default()), 1);
    }

    #[test]
    fn watch_limit_warns_at_the_limit() {
        assert_eq!(limit_finding(8_192, 100).outcome, Outcome::Ok);
        let f = limit_finding(8_192, 10_000);
        assert_eq!(f.outcome, Outcome::Warn);
        assert_eq!(
            f.fix.as_deref(),
            Some("`sudo sysctl fs.inotify.max_user_watches=20000`")
        );
    }
}
