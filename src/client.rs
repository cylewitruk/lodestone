//! Blocking HTTP client the CLI uses when a daemon is running.

use std::time::Duration;

use anyhow::{Context, bail};
use serde::de::DeserializeOwned;

use crate::api::types::{
    ErrorBody, IndexRequest, IndexResponse, RegisterRequest, RepoInfo, SearchRequest,
    SearchResponse, StatusReport,
};

pub struct Client {
    base: String,
    agent: ureq::Agent,
}

impl Client {
    /// Returns a client if a daemon answers on `127.0.0.1:port`.
    pub fn connect(port: u16) -> Option<Self> {
        let probe: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(300)))
            .build()
            .into();
        let base = format!("http://127.0.0.1:{port}");
        probe.get(format!("{base}/v1/repos")).call().ok()?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(24 * 3600)))
            .http_status_as_error(false)
            .build()
            .into();
        Some(Self { base, agent })
    }

    fn decode<T: DeserializeOwned>(
        mut resp: ureq::http::Response<ureq::Body>,
    ) -> anyhow::Result<T> {
        let status = resp.status();
        let text = resp.body_mut().read_to_string()?;
        if !status.is_success() {
            let msg = serde_json::from_str::<ErrorBody>(&text)
                .map(|e| e.error)
                .unwrap_or(text);
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
}
