//! `lode doctor` against a test home, a sandboxed user home and a fake `codex` on PATH.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lodestone::testutil::tempdir;
use serde_json::Value;

use crate::daemon::{git, lode, on_demand_home, repo, wait_until};

/// A user home, and a PATH holding only a `codex` that does nothing.
struct Sandbox {
    home: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let base = tempdir();
        let (home, bin) = (base.join("home"), base.join("bin"));
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let codex = bin.join("codex");
        fs::write(&codex, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
        Self { home, bin }
    }

    /// `lode <args>` in `dir` with lodestone home `lodestone`.
    fn lode(&self, lodestone: &Path, dir: &Path, args: &[&str]) -> std::process::Output {
        lode(lodestone)
            .args(args)
            .current_dir(dir)
            .env("HOME", &self.home)
            .env("PATH", &self.bin)
            .env("LODESTONE_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap()
    }

    /// `lode doctor --json` in `dir`: whether it passed, and its report.
    fn doctor(&self, lodestone: &Path, dir: &Path) -> (bool, Value) {
        let out = self.lode(lodestone, dir, &["doctor", "--json"]);
        let report = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!("{e}: {}", String::from_utf8_lossy(&out.stderr));
        });
        (out.status.success(), report)
    }
}

/// The check named `name` in `report`.
fn check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no {name} check in {report:#}"))
}

#[test]
fn doctor_reports_each_check_and_fails_on_a_broken_install() {
    let sandbox = Sandbox::new();
    let home = on_demand_home(60);
    let root = repo(3);
    git(&root, &["init", "-q"]);

    // Nothing set up: warnings, with fixes, but nothing failed.
    let (ok, r) = sandbox.doctor(&home, &root);
    assert!(ok, "{r:#}");
    assert_eq!(check(&r, "daemon")["outcome"], "ok", "{r:#}");
    assert_eq!(check(&r, "repository")["fix"], "`lode register`", "{r:#}");
    assert_eq!(check(&r, "codex")["outcome"], "warn", "{r:#}");
    assert_eq!(check(&r, "codex")["fix"], "`lode install --client codex`");
    for skipped in ["claude", "antigravity", "version"] {
        assert_eq!(check(&r, skipped)["outcome"], "skip", "{r:#}");
    }

    let out = sandbox.lode(&home, &root, &["install", "--client", "codex"]);
    assert!(out.status.success(), "{out:?}");
    let out = sandbox.lode(&home, &root, &["register"]);
    assert!(out.status.success(), "{out:?}");

    let version = format!("lode {}", env!("CARGO_PKG_VERSION"));
    let mut last = Value::Null;
    wait_until("doctor to pass", Duration::from_secs(30), || {
        let (ok, r) = sandbox.doctor(&home, &root);
        last = r;
        ok && check(&last, "repository")["outcome"] == "ok"
            && check(&last, "daemon")["detail"]
                .as_str()
                .is_some_and(|d| d.starts_with(&version))
    });
    assert_eq!(check(&last, "model")["outcome"], "ok", "{last:#}");
    assert_eq!(
        check(&last, "codex")["detail"],
        "user scope: skill, MCP server, SessionStart hook",
        "{last:#}"
    );

    // Another `lode`'s MCP server.
    let config = sandbox.home.join(".codex/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    let start = text.find("command = ").unwrap();
    let end = start + text[start..].find('\n').unwrap();
    fs::write(
        &config,
        format!("{}command = \"/old/lode\"{}", &text[..start], &text[end..]),
    )
    .unwrap();
    let (ok, r) = sandbox.doctor(&home, &root);
    assert!(!ok, "{r:#}");
    let codex = check(&r, "codex");
    assert_eq!(codex["outcome"], "fail", "{r:#}");
    assert!(
        codex["detail"]
            .as_str()
            .unwrap()
            .contains("outdated: MCP server"),
        "{r:#}"
    );
    assert_eq!(codex["fix"], "`lode install --client codex`");

    sandbox.lode(&home, &root, &["stop"]);
}
