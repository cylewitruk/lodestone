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
    /// Stable model identity used in the embedding cache key (see [`model_id`]).
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn backend(&self) -> Backend;
    /// Embeds `texts` in order. Implementations batch internally.
    fn embed(&self, texts: &[&str], kind: InputKind) -> anyhow::Result<Vec<Vec<f32>>>;
}

/// The identity of a model's document vectors, independent of the runtime executing it:
/// `hf_repo[@revision]#<fingerprint>` of the settings that shape the vectors. The built-in
/// models as v0.1.1 defined them keep the bare `hf_repo[@revision]` their vectors were cached
/// under; any other settings, including later changes to a built-in, get their own id.
pub fn model_id(model: &ModelConfig) -> String {
    let base = match &model.revision {
        Some(rev) => format!("{}@{rev}", model.hf_repo),
        None => model.hf_repo.clone(),
    };
    let settings = vector_settings(model);
    let legacy = blake3::hash(format!("{base}\n{settings}").as_bytes());
    if V0_1_1_BUILTINS.contains(&legacy.to_hex().as_str()) {
        base
    } else {
        format!(
            "{base}#{}",
            &blake3::hash(settings.as_bytes()).to_hex()[..12]
        )
    }
}

/// blake3 of `"{hf_repo[@revision]}\n{vector_settings}"` for each built-in model of v0.1.1, whose
/// ids had no fingerprint. Never add to it: a new or changed built-in gets a fingerprinted id.
const V0_1_1_BUILTINS: [&str; 13] = [
    "c55f43166ca381b88df54e7ced1a33950af158d7140a4678640690969f8df02e",
    "4b74252a75b8b8f24d82660e908eb796256bd0c34ed15d99976c622b8562f564",
    "76b5bdac9d7181e2292670f018923e775eaa00fba03b440722eb588df2d9f590",
    "6adb550b4aac33ebdda6270fc24f77f09d649a0046baef32822a1d931920a0d1",
    "b917e5aaeaae48210e45d541219094531319235b76d11b36bbc10aaed5ae871f",
    "e25b076eac3a8683c5b1781ed93d6675abc4f4c7b322ad941736c9bd1912a50d",
    "21a1a46c57ab27583ef737c6f0766dc2a3346c49fbde0f009c309ad890572131",
    "34bdde81ef2551fbbf1bfbc64874e88e5a397dbac8264ae77c5498d6f615c806",
    "7c2412d9138f2e69a335b48df4dfb5ca1647eef9725ac3340487956758267f4a",
    "d91a4cb22d32d1cd02e3052d9c4227e24556aebaf77b39b14baba8b654dced9f",
    "3b2c26fd60ea0b14b474b8576d17648943a7952a1a76454ab5210ec903eeb55a",
    "1ef4aa5810a0c7714b38c4a3ecc805279b54661007b97e3034097c825f94b71e",
    "3666d4af12031e17ebf16020682dced8f9b8b93854c00f338f5c2c2aef911419",
];

/// The settings that determine a document's vector besides the repository's weights, including
/// the fastembed variant, whose weights fastembed loads instead. The query prefix only shapes
/// queries, and the dtype is how the same model runs.
fn vector_settings(m: &ModelConfig) -> String {
    serde_json::json!([
        m.dim,
        m.max_tokens,
        m.arch,
        m.pooling,
        m.document_prefix,
        m.tokenizer_repo,
        m.fastembed
    ])
    .to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_models;

    #[test]
    fn model_id_changes_with_the_settings_that_shape_document_vectors() {
        // v0.1.1's built-ins, unchanged since; a change to one fails here, as it should give it
        // a fingerprinted id (then compare against a frozen v0.1.1 definition instead).
        for builtin in builtin_models().values() {
            assert!(!model_id(builtin).contains('#'), "{}", builtin.hf_repo);
        }
        let model = builtin_models().into_values().next().unwrap();
        let base = model_id(&model);
        let changed = |f: &dyn Fn(&mut ModelConfig)| {
            let mut m = model.clone();
            f(&mut m);
            model_id(&m)
        };
        assert_eq!(changed(&|m| m.query_prefix.push('x')), base);
        let prefix = changed(&|m| m.document_prefix.push('x'));
        assert!(prefix.starts_with(&format!("{base}#")), "{prefix}");
        assert_ne!(changed(&|m| m.max_tokens += 1), prefix);
        assert_ne!(changed(&|m| m.fastembed = Some("Other".into())), base);
        assert_eq!(changed(&|m| m.document_prefix.push('x')), prefix);
    }
}
