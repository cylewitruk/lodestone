//! Blocking HTTP client for the daemon, and starting an on-demand daemon when none runs.

#[cfg(unix)]
mod unix;

use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::de::DeserializeOwned;

use crate::api::types::{
    ErrorBody, IndexRequest, IndexResponse, RegisterRequest, RepoInfo, SearchRequest,
    SearchResponse, SessionRequest, SessionResponse, StatusOverview, StatusReport, ToolRequest,
};
use crate::config::{Config, Endpoint};

/// How long to wait for a daemon to start: loading the model takes seconds, longer on CPU with a
/// large model.
pub const START_WAIT: Duration = Duration::from_secs(120);

/// Starts `lode serve --on-demand` in the background, detached from the caller's process group
/// and logging to `<data dir>/daemon.log`. Losing a race with another starter is harmless: the
/// second daemon cannot bind the port and exits.
pub fn start_daemon(config: &Config, config_path: Option<&Path>) -> anyhow::Result<Child> {
    let log_path = config.data_dir().join("daemon.log");
    std::fs::create_dir_all(config.data_dir())?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    if let Some(p) = config_path {
        cmd.arg("--config").arg(p);
    }
    cmd.args(["serve", "--on-demand"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().context("starting the lodestone daemon")
}

/// Whether some daemon holds the socket's lock, i.e. is running or still loading its model.
#[cfg(unix)]
fn daemon_starting(config: &Config) -> bool {
    std::fs::File::open(config.lock_path())
        .is_ok_and(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
}

#[cfg(not(unix))]
fn daemon_starting(_: &Config) -> bool {
    true
}

/// Waits up to `timeout` for the daemon to exit: on Unix until it releases its socket's lock,
/// elsewhere until it stops answering.
pub fn wait_until_stopped(config: &Config, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        #[cfg(unix)]
        let alive = daemon_starting(config);
        #[cfg(not(unix))]
        let alive = Client::connect(&config.endpoint()).is_some();
        if !alive {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the lodestone daemon is still running after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Connects to the daemon, starting one on demand and waiting up to `wait` for it. Fails early
/// when the started daemon exits and no other daemon is starting (a lost race is not a failure).
pub fn connect_or_start(
    config: &Config,
    config_path: Option<&Path>,
    wait: Duration,
) -> anyhow::Result<Client> {
    let endpoint = config.endpoint();
    if let Some(c) = Client::connect(&endpoint) {
        return Ok(c);
    }
    let mut child = start_daemon(config, config_path)?;
    let log = config.data_dir().join("daemon.log");
    let deadline = Instant::now() + wait;
    loop {
        if let Some(c) = Client::connect(&endpoint) {
            return Ok(c);
        }
        if let Some(status) = child.try_wait()?
            && !daemon_starting(config)
        {
            bail!(
                "the lodestone daemon exited ({status}); see {}",
                log.display()
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "the lodestone daemon did not start within {wait:?}; see {}",
                log.display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub struct Client {
    base: String,
    agent: ureq::Agent,
}

impl Client {
    /// Returns a client if a daemon answers at `endpoint`.
    pub fn connect(endpoint: &Endpoint) -> Option<Self> {
        let probe = Self::agent(endpoint, Duration::from_millis(300));
        let base = match endpoint {
            #[cfg(unix)]
            Endpoint::Unix(_) => "http://localhost".to_string(),
            Endpoint::Tcp(port) => format!("http://127.0.0.1:{port}"),
        };
        probe.get(format!("{base}/v1/repos")).call().ok()?;
        let agent = Self::agent(endpoint, Duration::from_secs(24 * 3600));
        Some(Self { base, agent })
    }

    fn agent(endpoint: &Endpoint, timeout: Duration) -> ureq::Agent {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();
        match endpoint {
            #[cfg(unix)]
            Endpoint::Unix(path) => ureq::Agent::with_parts(
                config,
                unix::UnixConnector(path.clone()),
                ureq::unversioned::resolver::DefaultResolver::default(),
            ),
            Endpoint::Tcp(_) => config.into(),
        }
    }

    fn decode<T: DeserializeOwned>(
        mut resp: ureq::http::Response<ureq::Body>,
    ) -> anyhow::Result<T> {
        let status = resp.status();
        let text = resp.body_mut().read_to_string()?;
        if !status.is_success() {
            let msg = match serde_json::from_str::<ErrorBody>(&text) {
                Ok(e) => e.error,
                // The daemon's own errors carry a body; an empty one is an unknown route.
                Err(_) if text.is_empty() => format!(
                    "the daemon answered {status}; it may predate this `lode` (`lode stop` it)"
                ),
                Err(_) => text,
            };
            bail!("{msg}");
        }
        serde_json::from_str(&text).context("decoding daemon response")
    }

    pub fn search(&self, req: &SearchRequest) -> anyhow::Result<SearchResponse> {
        Self::decode(
            self.agent
                .post(format!("{}/v1/search", self.base))
                .send_json(req)?,
        )
    }

    pub fn status(&self, repo: Option<&str>) -> anyhow::Result<StatusReport> {
        let mut r = self.agent.get(format!("{}/v1/status", self.base));
        if let Some(repo) = repo {
            r = r.query("repo", repo);
        }
        Self::decode(r.call()?)
    }

    pub fn status_all(&self) -> anyhow::Result<StatusOverview> {
        Self::decode(
            self.agent
                .get(format!("{}/v1/status/all", self.base))
                .call()?,
        )
    }

    pub fn register(&self, req: &RegisterRequest) -> anyhow::Result<RepoInfo> {
        Self::decode(
            self.agent
                .post(format!("{}/v1/register", self.base))
                .send_json(req)?,
        )
    }

    pub fn index(&self, req: &IndexRequest) -> anyhow::Result<IndexResponse> {
        Self::decode(
            self.agent
                .post(format!("{}/v1/index", self.base))
                .send_json(req)?,
        )
    }

    pub fn repos(&self) -> anyhow::Result<Vec<RepoInfo>> {
        Self::decode(self.agent.get(format!("{}/v1/repos", self.base)).call()?)
    }

    pub fn session(&self, req: &SessionRequest) -> anyhow::Result<SessionResponse> {
        Self::decode(
            self.agent
                .post(format!("{}/v1/sessions", self.base))
                .send_json(req)?,
        )
    }

    /// Asks the daemon to shut down; see [`wait_until_stopped`].
    pub fn stop(&self) -> anyhow::Result<()> {
        let resp = self
            .agent
            .post(format!("{}/v1/shutdown", self.base))
            .send_empty()?;
        if !resp.status().is_success() {
            bail!("the daemon refused to stop: {}", resp.status());
        }
        Ok(())
    }

    pub fn release(&self, id: &str) -> anyhow::Result<()> {
        self.agent
            .delete(format!("{}/v1/sessions/{id}", self.base))
            .call()?;
        Ok(())
    }

    /// Runs an MCP tool: `Err` when the daemon cannot be reached, `Ok(Err(message))` when the
    /// tool itself fails.
    pub fn tool(&self, req: &ToolRequest) -> anyhow::Result<Result<String, String>> {
        let mut resp = self
            .agent
            .post(format!("{}/v1/tool", self.base))
            .send_json(req)?;
        let ok = resp.status().is_success();
        let text = resp.body_mut().read_to_string()?;
        Ok(if ok {
            Ok(text)
        } else {
            Err(serde_json::from_str::<ErrorBody>(&text)
                .map(|e| e.error)
                .unwrap_or(text))
        })
    }
}
