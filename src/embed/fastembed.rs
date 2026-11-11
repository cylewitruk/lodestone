//! fastembed (ONNX Runtime) backend: CPU everywhere, the Phase 0 baseline.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, bail};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use super::{Embedder, InputKind, model_id, prefixed};
use crate::config::{Backend, ModelConfig};

pub struct FastembedEmbedder {
    model: Mutex<TextEmbedding>,
    config: ModelConfig,
    id: String,
    batch_size: usize,
}

/// Maps a config's `fastembed` variant name to fastembed's enum.
fn variant(name: &str) -> anyhow::Result<EmbeddingModel> {
    Ok(match name {
        "JinaEmbeddingsV2BaseCode" => EmbeddingModel::JinaEmbeddingsV2BaseCode,
        "BGESmallENV15" => EmbeddingModel::BGESmallENV15,
        "BGEBaseENV15" => EmbeddingModel::BGEBaseENV15,
        "NomicEmbedTextV15" => EmbeddingModel::NomicEmbedTextV15,
        "AllMiniLML6V2" => EmbeddingModel::AllMiniLML6V2,
        other => bail!("unsupported fastembed model {other}"),
    })
}

impl FastembedEmbedder {
    pub fn load(config: &ModelConfig, cache_dir: &Path, batch_size: usize) -> anyhow::Result<Self> {
        let name = config
            .fastembed
            .as_deref()
            .with_context(|| format!("{} has no fastembed equivalent", config.hf_repo))?;
        let opts = TextInitOptions::new(variant(name)?)
            .with_cache_dir(cache_dir.join("fastembed"))
            .with_max_length(config.max_tokens)
            .with_show_download_progress(false);
        // `LODESTONE_FASTEMBED_EP=coreml` routes ONNX Runtime through CoreML on macOS.
        #[cfg(target_os = "macos")]
        let opts = if std::env::var("LODESTONE_FASTEMBED_EP").as_deref() == Ok("coreml") {
            opts.with_execution_providers(vec![ort::ep::CoreML::default().build()])
        } else {
            opts
        };
        Ok(Self {
            model: Mutex::new(TextEmbedding::try_new(opts)?),
            config: config.clone(),
            id: model_id(config),
            batch_size,
        })
    }
}

impl Embedder for FastembedEmbedder {
    fn model_id(&self) -> &str {
        &self.id
    }

    fn dim(&self) -> usize {
        self.config.dim
    }

    fn backend(&self) -> Backend {
        Backend::Fastembed
    }

    fn embed(&self, texts: &[&str], kind: InputKind) -> anyhow::Result<Vec<Vec<f32>>> {
        let inputs = prefixed(&self.config, texts, kind);
        let inputs: Vec<&str> = inputs.iter().map(|c| c.as_ref()).collect();
        let mut model = self
            .model
            .lock()
            .map_err(|_| anyhow::anyhow!("model lock poisoned"))?;
        let mut out = model.embed(inputs, Some(self.batch_size))?;
        out.iter_mut().for_each(|v| super::normalize(v));
        Ok(out)
    }
}
