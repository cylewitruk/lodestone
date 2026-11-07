//! candle backend (Metal on macOS, CPU elsewhere). Models load from safetensors via hf-hub.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, bail};
use candle_core::{D, DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::{bert, nomic_bert};
use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Repo, RepoType};
use tokenizers::{Tokenizer, TruncationParams};

use super::jina::{JinaBert, JinaConfig};
use super::modernbert::{self, ModernBertEncoder};
use super::{Embedder, InputKind, model_id, prefixed};
use crate::config::{Arch, Backend, ModelConfig, Pooling};

enum Model {
    Bert(bert::BertModel),
    Jina(JinaBert),
    Nomic(nomic_bert::NomicBertModel),
    Modern(ModernBertEncoder),
}

pub struct CandleEmbedder {
    model: Mutex<Model>,
    tokenizer: Tokenizer,
    device: Device,
    config: ModelConfig,
    id: String,
    backend: Backend,
    batch_size: usize,
}

impl CandleEmbedder {
    pub fn load(
        config: &ModelConfig,
        backend: Backend,
        cache_dir: &Path,
        batch_size: usize,
    ) -> anyhow::Result<Self> {
        let device = match backend {
            Backend::CandleMetal => Device::new_metal(0).context("Metal device unavailable")?,
            _ => Device::Cpu,
        };
        let dtype = match std::env::var("LODESTONE_DTYPE").as_deref() {
            Ok("f16") => DType::F16,
            Ok("f32") => DType::F32,
            _ if config.dtype == Some(crate::config::Precision::F32) => DType::F32,
            _ if device.is_metal() => DType::F16,
            _ => DType::F32,
        };
        let api = ApiBuilder::new()
            .with_cache_dir(cache_dir.to_path_buf())
            .with_progress(false)
            .build()?;
        let repo = api.repo(match &config.revision {
            Some(r) => Repo::with_revision(config.hf_repo.clone(), RepoType::Model, r.clone()),
            None => Repo::new(config.hf_repo.clone(), RepoType::Model),
        });
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("downloading tokenizer.json")?;
        let config_path = repo.get("config.json").context("downloading config.json")?;
        let weights = repo
            .get("model.safetensors")
            .context("downloading model.safetensors")?;
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(anyhow::Error::msg)?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: config.max_tokens,
                ..Default::default()
            }))
            .map_err(anyhow::Error::msg)?;
        tokenizer.with_padding(None);
        let cfg_text = std::fs::read_to_string(&config_path)?;
        // SAFETY: the safetensors file is not modified while mapped (hf-hub cache is immutable).
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights], dtype, &device)? };
        let model = match config.arch {
            Arch::Bert => Model::Bert(bert::BertModel::load(
                vb,
                &serde_json::from_str(&cfg_text)?,
            )?),
            Arch::JinaBert => {
                let cfg: JinaConfig = serde_json::from_str(&cfg_text)?;
                Model::Jina(JinaBert::new(vb, &cfg, config.max_tokens)?)
            }
            Arch::NomicBert => Model::Nomic(nomic_bert::NomicBertModel::load(
                vb,
                &serde_json::from_str(&cfg_text)?,
            )?),
            Arch::ModernBert => {
                let cfg: modernbert::Config = serde_json::from_str(&cfg_text)?;
                Model::Modern(ModernBertEncoder::load(vb, &cfg, config.max_tokens + 8)?)
            }
        };
        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            device,
            config: config.clone(),
            id: model_id(config),
            backend,
            batch_size: batch_size.max(1),
        })
    }

    fn embed_batch(&self, ids: &[&[u32]]) -> anyhow::Result<Vec<Vec<f32>>> {
        let len = ids.iter().map(|i| i.len()).max().unwrap_or(1).max(1);
        let mut flat_ids = Vec::with_capacity(ids.len() * len);
        let mut flat_mask = Vec::with_capacity(ids.len() * len);
        for row in ids {
            flat_ids.extend_from_slice(row);
            flat_ids.extend(std::iter::repeat_n(0u32, len - row.len()));
            flat_mask.extend(std::iter::repeat_n(1u32, row.len()));
            flat_mask.extend(std::iter::repeat_n(0u32, len - row.len()));
        }
        let shape = (ids.len(), len);
        let input = Tensor::from_vec(flat_ids, shape, &self.device)?;
        let mask = Tensor::from_vec(flat_mask, shape, &self.device)?;
        let hidden = {
            let model = self
                .model
                .lock()
                .map_err(|_| anyhow::anyhow!("model lock poisoned"))?;
            match &*model {
                Model::Bert(m) => m.forward(&input, &input.zeros_like()?, Some(&mask))?,
                Model::Jina(m) => m.forward(&input, &mask)?,
                // Metal has no u32/f16 `where_cond`; u8 masks are supported.
                Model::Nomic(m) => m.forward(&input, None, Some(&mask.to_dtype(DType::U8)?))?,
                Model::Modern(m) => m.forward(&input, &mask)?,
            }
        };
        let hidden = hidden.to_dtype(DType::F32)?;
        let pooled = match self.config.pooling {
            Pooling::Cls => hidden.narrow(1, 0, 1)?.squeeze(1)?,
            Pooling::Mean => {
                let m = mask.to_dtype(DType::F32)?.unsqueeze(D::Minus1)?;
                let sum = hidden.broadcast_mul(&m)?.sum(1)?;
                sum.broadcast_div(&m.sum(1)?)?
            }
        };
        let norm = pooled.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normed = pooled.broadcast_div(&norm)?;
        Ok(normed.to_vec2::<f32>()?)
    }
}

impl Embedder for CandleEmbedder {
    fn model_id(&self) -> &str {
        &self.id
    }

    fn dim(&self) -> usize {
        self.config.dim
    }

    fn backend(&self) -> Backend {
        self.backend
    }

    fn embed(&self, texts: &[&str], kind: InputKind) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let inputs: Vec<String> = prefixed(&self.config, texts, kind)
            .into_iter()
            .map(|c| c.into_owned())
            .collect();
        let encodings = self
            .tokenizer
            .encode_batch(inputs, true)
            .map_err(anyhow::Error::msg)?;
        // Sort by length so each batch pads as little as possible.
        let mut order: Vec<usize> = (0..encodings.len()).collect();
        order.sort_by_key(|&i| encodings[i].get_ids().len());
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        // Keep each batch under a token budget so long inputs do not blow up attention memory.
        let budget = self.batch_size * 256;
        let mut start = 0;
        while start < order.len() {
            let mut end = start;
            while end < order.len() && end - start < self.batch_size {
                let longest = encodings[order[end]].get_ids().len().max(1);
                if end > start && longest * (end - start + 1) > budget {
                    break;
                }
                end += 1;
            }
            let batch: Vec<&[u32]> = order[start..end]
                .iter()
                .map(|&i| encodings[i].get_ids())
                .collect();
            let vecs = self.embed_batch(&batch)?;
            for (&i, v) in order[start..end].iter().zip(vecs) {
                out[i] = v;
            }
            start = end;
        }
        if out.iter().flatten().any(|x| !x.is_finite()) {
            bail!(
                "model {} produced non-finite embeddings (f16 overflow?); set `dtype = \"f32\"` for it under [models]",
                self.id
            );
        }
        if out.iter().any(|v| v.len() != self.config.dim) {
            bail!("model {} returned vectors of unexpected dimension", self.id);
        }
        Ok(out)
    }
}
