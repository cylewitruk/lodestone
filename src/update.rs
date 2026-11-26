//! New releases: the CLI's notice that one is out, and `lode upgrade` through the cargo-dist
//! installer that installed this binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Context;
use axoupdater::AxoUpdater;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::client;
use crate::config::Config;
use crate::text;

/// The package that cargo-dist's installers, receipt and releases are named after.
const APP: &str = env!("CARGO_PKG_NAME");
/// How often the CLI looks up the latest release.
const CHECK_INTERVAL_SECS: u64 = 24 * 3600;
/// How long a lookup of the latest release may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// Set to anything to turn off the lookup and the notice.
pub const OPT_OUT_ENV: &str = "LODESTONE_NO_UPDATE_CHECK";

/// The last lookup of the latest release, in `<data dir>/update-check.json`.
#[derive(Default, Serialize, Deserialize)]
struct Cache {
    /// Unix time of the last lookup started, whether or not it succeeded.
    checked_at: u64,
    latest: Option<String>,
}

impl Cache {
    fn path(config: &Config) -> PathBuf {
        config.data_dir().join("update-check.json")
    }

    /// Whether a new lookup is due.
    fn stale(&self, now: u64) -> bool {
        now.saturating_sub(self.checked_at) >= CHECK_INTERVAL_SECS
    }

    fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Atomic, so concurrent commands read a whole cache: a temp file per process, then rename.
    fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        fs::write(&tmp, serde_json::to_vec(self)?)?;
        fs::rename(&tmp, path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(())
    }
}

/// This binary's version.
pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is semver")
}

/// The one-line notice to show when a newer release than this binary is known, unless
/// [`OPT_OUT_ENV`] is set. Never waits on the network: when the last lookup is a day old, starts
/// another in the background ([`check`] in a detached `lode update-check`) for later commands.
pub fn notice(config: &Config, config_path: Option<&Path>) -> Option<String> {
    if std::env::var_os(OPT_OUT_ENV).is_some() {
        return None;
    }
    let path = Cache::path(config);
    let mut cache = Cache::load(&path);
    let now = text::now();
    // One command at a time decides; it records the lookup before starting it, so commands run
    // meanwhile, or after it fails, start none.
    if cache.stale(now)
        && fs::create_dir_all(config.data_dir()).is_ok()
        && let Ok(lock) = fs::File::create(path.with_extension("lock"))
        && lock.try_lock().is_ok()
    {
        cache = Cache::load(&path);
        if cache.stale(now) {
            cache.checked_at = now;
            if cache.save(&path).is_ok() {
                let _ = spawn_check(config_path);
            }
        }
    }
    let latest = Version::parse(cache.latest.as_deref()?).ok()?;
    (latest > current()).then(|| {
        format!(
            "lode {latest} is available (this is {}); run `lode upgrade`",
            current()
        )
    })
}

fn spawn_check(config_path: Option<&Path>) -> std::io::Result<()> {
    let mut cmd = Command::new(client::lode_exe()?);
    if let Some(p) = config_path {
        cmd.arg("--config").arg(p);
    }
    cmd.arg("update-check")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    client::spawn_detached(&mut cmd).map(drop)
}

/// Looks up the latest GitHub release and records it for [`notice`].
pub fn check(config: &Config) -> anyhow::Result<Version> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let repo = env!("CARGO_PKG_REPOSITORY")
        .strip_prefix("https://github.com/")
        .context("the package repository is not on GitHub")?;
    let release: Release = ureq::get(format!(
        "https://api.github.com/repos/{repo}/releases/latest"
    ))
    .header("Accept", "application/vnd.github+json")
    .header(
        "User-Agent",
        concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
    )
    .config()
    .timeout_global(Some(CHECK_TIMEOUT))
    .build()
    .call()
    .context("looking up the latest lodestone release")?
    .body_mut()
    .read_json()?;
    let latest = parse_tag(&release.tag_name)?;
    record(config, &latest);
    Ok(latest)
}

fn parse_tag(tag: &str) -> anyhow::Result<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .with_context(|| format!("release tag {tag} is not a version"))
}

/// Records `latest` as the latest release, as of now. Best effort: a later lookup repairs it.
fn record(config: &Config, latest: &Version) {
    let cache = Cache {
        checked_at: text::now(),
        latest: Some(latest.to_string()),
    };
    let _ = cache.save(&Cache::path(config));
}

/// What [`upgrade`] did.
pub enum Upgrade {
    /// Installed `to`; `stopped_daemon` when a running daemon was stopped for it.
    Upgraded { to: Version, stopped_daemon: bool },
    /// Already the latest release.
    UpToDate,
    /// `latest` is out, but this binary was not installed by the lodestone installer (e.g.
    /// `cargo install`), so it must be upgraded the way it was installed.
    Unmanaged { latest: Version },
}

/// Upgrades to the latest release with its cargo-dist installer, into the directory the install
/// receipt names, then stops a running daemon so the next command starts the new one.
pub fn upgrade(config: &Config) -> anyhow::Result<Upgrade> {
    let mut updater = AxoUpdater::new_for(APP);
    let managed = updater.load_receipt().is_ok()
        && updater
            .check_receipt_is_for_this_executable()
            .unwrap_or(false);
    if !managed {
        let latest = check(config)?;
        return Ok(if latest > current() {
            Upgrade::Unmanaged { latest }
        } else {
            Upgrade::UpToDate
        });
    }
    // The receipt's version is stale when another install (e.g. `cargo install`) replaced the
    // binary in the same place.
    updater.set_current_version(current())?;
    if !updater
        .is_update_needed_sync()
        .context("looking up the latest lodestone release")?
    {
        return Ok(Upgrade::UpToDate);
    }
    let result = updater
        .run_sync()
        .context("running the lodestone installer")?
        .context("the installer found nothing to upgrade")?;
    // Only once the new binary is in place: until then, an agent session's `lode mcp` would
    // restart the old daemon. Every daemon started from now on is the new one.
    let stopped_daemon = client::stop_daemon(config)?;
    record(config, &result.new_version);
    Ok(Upgrade::Upgraded {
        to: result.new_version,
        stopped_daemon,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_tags() {
        assert_eq!(parse_tag("v0.1.4").unwrap(), Version::new(0, 1, 4));
        assert_eq!(parse_tag("1.2.3").unwrap(), Version::new(1, 2, 3));
        assert!(parse_tag("latest").is_err());
    }

    #[test]
    fn notice_only_for_newer_releases() {
        let config = Config {
            data_dir: Some(crate::testutil::tempdir()),
            ..Config::default()
        };
        let path = Cache::path(&config);
        let notice_for = |latest: Version| {
            // A fresh lookup, so `notice` starts none.
            Cache {
                checked_at: text::now(),
                latest: Some(latest.to_string()),
            }
            .save(&path)
            .unwrap();
            notice(&config, None)
        };
        let mut newer = current();
        newer.patch += 1;
        let msg = notice_for(newer.clone()).expect("a notice for a newer release");
        assert!(msg.contains(&newer.to_string()), "{msg}");
        assert_eq!(notice_for(current()), None);
        assert_eq!(notice_for(Version::new(0, 0, 1)), None);
    }
}
