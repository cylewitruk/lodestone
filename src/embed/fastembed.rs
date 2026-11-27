//! fastembed (ONNX Runtime) backend: CPU everywhere, the Phase 0 baseline.

use std::fs;
use std::iter;
use std::path::{Path, PathBuf};
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

/// fastembed's model for `config`.
fn model(config: &ModelConfig) -> anyhow::Result<EmbeddingModel> {
    let name = config
        .fastembed
        .as_deref()
        .with_context(|| format!("{} has no fastembed equivalent", config.hf_repo))?;
    variant(name)
}

/// Where fastembed keeps its downloads under lodestone's model cache.
fn files_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("fastembed")
}

/// The files of `config` that [`FastembedEmbedder::load`] would download first into `cache_dir`.
pub fn missing_files(config: &ModelConfig, cache_dir: &Path) -> anyhow::Result<Vec<String>> {
    let model = model(config)?;
    let info = TextEmbedding::get_model_info(&model)?;
    // hf-hub's layout: `models--<org>--<name>/snapshots/<revision>/<file>`.
    let snapshots = files_dir(cache_dir)
        .join(format!("models--{}", info.model_code.replace('/', "--")))
        .join("snapshots");
    let snapshots: Vec<PathBuf> = fs::read_dir(&snapshots)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    Ok(iter::once(info.model_file.as_str())
        .chain(info.additional_files.iter().map(String::as_str))
        .chain(TOKENIZER_FILES)
        .filter(|f| !snapshots.iter().any(|s| s.join(f).exists()))
        .map(String::from)
        .collect())
}

/// What fastembed's `load_tokenizer_hf_hub` reads besides its `ModelInfo`'s files (as of
/// fastembed 7.6).
const TOKENIZER_FILES: [&str; 4] = [
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];

impl FastembedEmbedder {
    pub fn load(config: &ModelConfig, cache_dir: &Path, batch_size: usize) -> anyhow::Result<Self> {
        let opts = TextInitOptions::new(model(config)?)
            .with_cache_dir(files_dir(cache_dir))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_models;

    #[test]
    fn a_cache_without_the_tokenizer_files_is_missing_them() {
        let config = builtin_models().remove("jina-code-v2").unwrap();
        let cache_dir = crate::testutil::tempdir();
        let model = model(&config).unwrap();
        let info = TextEmbedding::get_model_info(&model).unwrap();
        let snapshot = files_dir(&cache_dir)
            .join(format!("models--{}", info.model_code.replace('/', "--")))
            .join("snapshots/0123abc");
        for file in iter::once(&info.model_file).chain(&info.additional_files) {
            let path = snapshot.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        assert_eq!(missing_files(&config, &cache_dir).unwrap(), TOKENIZER_FILES);
        for file in TOKENIZER_FILES {
            fs::write(snapshot.join(file), "").unwrap();
        }
        assert!(missing_files(&config, &cache_dir).unwrap().is_empty());
    }
}
