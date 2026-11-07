//! OpenAI-compatible `/v1/embeddings` endpoint.

use anyhow::Context;
use serde::Deserialize;

use super::{Embedder, InputKind, model_id, prefixed};
use crate::config::{Backend, ModelConfig, RemoteConfig};

pub struct RemoteEmbedder {
    config: ModelConfig,
    id: String,
    url: String,
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Datum>,
}

#[derive(Deserialize)]
struct Datum {
    index: usize,
    embedding: Vec<f32>,
}

impl RemoteEmbedder {
    pub fn new(config: &ModelConfig, remote: &RemoteConfig) -> anyhow::Result<Self> {
        Ok(Self {
            config: config.clone(),
            id: model_id(config),
            url: remote.url.clone(),
            api_key: std::env::var(&remote.api_key_env).ok(),
        })
    }
}

impl Embedder for RemoteEmbedder {
    fn model_id(&self) -> &str {
        &self.id
    }

    fn dim(&self) -> usize {
        self.config.dim
    }

    fn backend(&self) -> Backend {
        Backend::Remote
    }

    fn embed(&self, texts: &[&str], kind: InputKind) -> anyhow::Result<Vec<Vec<f32>>> {
        let input: Vec<String> = prefixed(&self.config, texts, kind)
            .into_iter()
            .map(|c| c.into_owned())
            .collect();
        let body = serde_json::json!({ "model": self.config.hf_repo, "input": input });
        let mut req = ureq::post(&self.url);
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        let resp: Response = req
            .send_json(&body)
            .context("remote embedding request")?
            .body_mut()
            .read_json()?;
        let mut out = vec![Vec::new(); texts.len()];
        for d in resp.data {
            if let Some(slot) = out.get_mut(d.index) {
                *slot = d.embedding;
                super::normalize(slot);
            }
        }
        Ok(out)
    }
}
