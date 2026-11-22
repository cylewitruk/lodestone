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
        vectors(resp, texts.len(), self.config.dim)
    }
}

/// The response's vectors in input order, normalized; an error unless it has exactly one finite
/// vector of `dim` values per input.
fn vectors(resp: Response, n: usize, dim: usize) -> anyhow::Result<Vec<Vec<f32>>> {
    anyhow::ensure!(
        resp.data.len() == n,
        "remote embedding response has {} vectors for {n} inputs",
        resp.data.len()
    );
    let mut out: Vec<Option<Vec<f32>>> = vec![None; n];
    for d in resp.data {
        anyhow::ensure!(
            d.embedding.len() == dim && d.embedding.iter().all(|x| x.is_finite()),
            "remote embedding {} has {} values, expected {dim} finite ones",
            d.index,
            d.embedding.len()
        );
        let slot = out
            .get_mut(d.index)
            .with_context(|| format!("remote embedding index {} out of range", d.index))?;
        anyhow::ensure!(
            slot.is_none(),
            "remote embedding index {} repeated",
            d.index
        );
        let mut v = d.embedding;
        super::normalize(&mut v);
        *slot = Some(v);
    }
    // Every slot is filled: n unique indices below n.
    Ok(out.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(data: &[(usize, &[f32])]) -> Response {
        Response {
            data: data
                .iter()
                .map(|(index, v)| Datum {
                    index: *index,
                    embedding: v.to_vec(),
                })
                .collect(),
        }
    }

    #[test]
    fn incomplete_or_malformed_responses_are_errors() {
        let ok = vectors(response(&[(1, &[0.0, 2.0]), (0, &[3.0, 0.0])]), 2, 2).unwrap();
        assert_eq!(ok, [vec![1.0, 0.0], vec![0.0, 1.0]]);
        for (bad, why) in [
            (response(&[(0, &[1.0, 0.0])]), "missing"),
            (response(&[(0, &[1.0, 0.0]), (0, &[0.0, 1.0])]), "repeated"),
            (
                response(&[(0, &[1.0, 0.0]), (2, &[0.0, 1.0])]),
                "out of range",
            ),
            (response(&[(0, &[1.0]), (1, &[0.0, 1.0])]), "dimension"),
            (
                response(&[(0, &[f32::NAN, 0.0]), (1, &[0.0, 1.0])]),
                "non-finite",
            ),
        ] {
            assert!(vectors(bad, 2, 2).is_err(), "{why}");
        }
    }
}
