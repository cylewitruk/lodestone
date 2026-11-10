//! Embedding runtimes behind one trait. Vectors are L2-normalized f32.

#[cfg(feature = "candle")]
pub mod candle;
#[cfg(feature = "candle")]
pub mod decoder;
#[cfg(feature = "candle")]
pub mod embedding_gemma;
#[cfg(feature = "fastembed")]
pub mod fastembed;
#[cfg(feature = "candle")]
pub mod jina;
#[cfg(feature = "candle")]
pub mod layers;
#[cfg(feature = "candle")]
pub mod modernbert;
pub mod remote;
#[cfg(feature = "candle")]
pub mod ultrabert;

use std::sync::Arc;

use anyhow::bail;

use crate::config::{Backend, Config, ModelConfig};

/// Whether a text is a search query or an indexed document; some models use different
/// prefixes for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    Document,
    Query,
}

pub trait Embedder: Send + Sync {
    /// Stable model identity used in the embedding cache key (`hf_repo[@revision]`).
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn backend(&self) -> Backend;
    /// Embeds `texts` in order. Implementations batch internally.
    fn embed(&self, texts: &[&str], kind: InputKind) -> anyhow::Result<Vec<Vec<f32>>>;
}

/// The identity of a model independent of the runtime executing it.
pub fn model_id(model: &ModelConfig) -> String {
    match &model.revision {
        Some(rev) => format!("{}@{rev}", model.hf_repo),
        None => model.hf_repo.clone(),
    }
}

pub fn prefixed<'a>(
    model: &ModelConfig,
    texts: &'a [&'a str],
    kind: InputKind,
) -> Vec<std::borrow::Cow<'a, str>> {
    let prefix = match kind {
        InputKind::Document => &model.document_prefix,
        InputKind::Query => &model.query_prefix,
    };
    texts
        .iter()
        .map(|t| {
            if prefix.is_empty() {
                std::borrow::Cow::Borrowed(*t)
            } else {
                std::borrow::Cow::Owned(format!("{prefix}{t}"))
            }
        })
        .collect()
}

/// Loads `alias` from the config's model table on `backend`.
pub fn load(config: &Config, alias: &str, backend: Backend) -> anyhow::Result<Arc<dyn Embedder>> {
    let model = config.model_config(alias)?.clone();
    let cache_dir = config.data_dir().join("models");
    match backend {
        Backend::CandleMetal | Backend::CandleCpu => {
            #[cfg(feature = "candle")]
            {
                Ok(Arc::new(candle::CandleEmbedder::load(
                    &model,
                    backend,
                    &cache_dir,
                    config.batch_size,
                )?))
            }
            #[cfg(not(feature = "candle"))]
            {
                let _ = cache_dir;
                bail!("lodestone was built without the `candle` feature")
            }
        }
        Backend::Fastembed => {
            #[cfg(feature = "fastembed")]
            {
                Ok(Arc::new(fastembed::FastembedEmbedder::load(
                    &model,
                    &cache_dir,
                    config.batch_size.min(32),
                )?))
            }
            #[cfg(not(feature = "fastembed"))]
            {
                let _ = cache_dir;
                bail!("lodestone was built without the `fastembed` feature")
            }
        }
        Backend::Remote => Ok(Arc::new(remote::RemoteEmbedder::new(
            &model,
            &config.remote,
        )?)),
    }
}

/// Backends compiled into this binary.
pub fn available_backends() -> Vec<Backend> {
    let mut v = Vec::new();
    if cfg!(feature = "candle") {
        if cfg!(target_os = "macos") {
            v.push(Backend::CandleMetal);
        }
        v.push(Backend::CandleCpu);
    }
    if cfg!(feature = "fastembed") {
        v.push(Backend::Fastembed);
    }
    v.push(Backend::Remote);
    v
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Deterministic embedder for tests: a hashed bag of lowercase word trigrams.
pub struct HashEmbedder {
    pub dim: usize,
    pub id: String,
    /// Artificial per-call latency for document batches (tests of slow indexing).
    pub delay_ms: u64,
}

impl Embedder for HashEmbedder {
    fn model_id(&self) -> &str {
        &self.id
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn backend(&self) -> Backend {
        Backend::Remote
    }
    fn embed(&self, texts: &[&str], kind: InputKind) -> anyhow::Result<Vec<Vec<f32>>> {
        if kind == InputKind::Document && self.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
        }
        if self.dim == 0 {
            bail!("zero dim");
        }
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; self.dim];
                let lower = t.to_lowercase();
                for w in lower
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| !w.is_empty())
                {
                    let h = blake3::hash(w.as_bytes());
                    let idx = u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap()) as usize
                        % self.dim;
                    v[idx] += 1.0;
                }
                normalize(&mut v);
                v
            })
            .collect())
    }
}
