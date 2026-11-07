//! Cross-encoder reranking of the fused top candidates.

pub trait Reranker: Send + Sync {
    /// One relevance score per document, higher is better.
    fn score(&self, query: &str, documents: &[&str]) -> anyhow::Result<Vec<f32>>;
}

#[cfg(feature = "fastembed")]
pub mod fastembed_impl {
    use std::path::Path;
    use std::sync::Mutex;

    use anyhow::bail;
    use fastembed::{RerankInitOptions, RerankerModel, TextRerank};

    use super::Reranker;

    pub struct FastembedReranker {
        model: Mutex<TextRerank>,
    }

    impl FastembedReranker {
        pub fn load(name: &str, cache_dir: &Path) -> anyhow::Result<Self> {
            let model = match name {
                "bge-reranker-base" => RerankerModel::BGERerankerBase,
                "bge-reranker-v2-m3" => RerankerModel::BGERerankerV2M3,
                "jina-reranker-v1-turbo-en" => RerankerModel::JINARerankerV1TurboEn,
                "jina-reranker-v2-base-multilingual" => {
                    RerankerModel::JINARerankerV2BaseMultilingual
                }
                other => bail!("unknown reranker {other}"),
            };
            let opts = RerankInitOptions::new(model)
                .with_cache_dir(cache_dir.join("fastembed"))
                .with_show_download_progress(false);
            Ok(Self {
                model: Mutex::new(TextRerank::try_new(opts)?),
            })
        }
    }

    impl Reranker for FastembedReranker {
        fn score(&self, query: &str, documents: &[&str]) -> anyhow::Result<Vec<f32>> {
            let mut m = self
                .model
                .lock()
                .map_err(|_| anyhow::anyhow!("reranker lock poisoned"))?;
            let results = m.rerank(query, documents, false, None)?;
            let mut scores = vec![f32::MIN; documents.len()];
            for r in results {
                if let Some(s) = scores.get_mut(r.index) {
                    *s = r.score;
                }
            }
            Ok(scores)
        }
    }
}

#[cfg(feature = "candle")]
pub mod candle_impl {
    use std::path::Path;
    use std::sync::Mutex;

    use crate::embed::modernbert::{self, ModernBertEncoder};
    use anyhow::Context;
    use candle_core::{D, DType, Device, Module, Tensor};
    use candle_nn::{LayerNorm, Linear, VarBuilder};
    use candle_transformers::models::xlm_roberta::{Config, XLMRobertaForSequenceClassification};
    use hf_hub::api::sync::ApiBuilder;
    use hf_hub::{Repo, RepoType};
    use tokenizers::{EncodeInput, Tokenizer, TruncationParams, TruncationStrategy};

    use super::Reranker;
    use crate::config::Backend;

    /// ModernBERT cross-encoder with mean pooling and a dense→GELU→norm→linear head
    /// (`Alibaba-NLP/gte-reranker-modernbert-base`). candle's own classification head ends in a
    /// softmax, which is constant for single-logit rerankers, so the head is applied here.
    pub struct ModernBertCrossEncoder {
        encoder: ModernBertEncoder,
        dense: Linear,
        norm: LayerNorm,
        classifier: Linear,
    }

    impl ModernBertCrossEncoder {
        fn load(
            vb: VarBuilder,
            cfg: &modernbert::Config,
            max_len: usize,
        ) -> candle_core::Result<Self> {
            let h = cfg.hidden_size;
            Ok(Self {
                encoder: ModernBertEncoder::load(vb.pp("model"), cfg, max_len)?,
                dense: candle_nn::linear_no_bias(h, h, vb.pp("head.dense"))?,
                norm: LayerNorm::new_no_bias(vb.get(h, "head.norm.weight")?, cfg.layer_norm_eps),
                classifier: candle_nn::linear(h, 1, vb.pp("classifier"))?,
            })
        }

        fn forward(&self, ids: &Tensor, mask: &Tensor) -> candle_core::Result<Tensor> {
            let hidden = self.encoder.forward(ids, mask)?;
            let m = mask.to_dtype(DType::F32)?.unsqueeze(D::Minus1)?;
            let hidden = hidden.to_dtype(DType::F32)?;
            let pooled = hidden
                .broadcast_mul(&m)?
                .sum(1)?
                .broadcast_div(&m.sum(1)?)?
                .to_dtype(self.dense.weight().dtype())?;
            let x = self
                .norm
                .forward(&self.dense.forward(&pooled)?.gelu_erf()?)?;
            self.classifier.forward(&x)
        }
    }

    enum Arch {
        XlmRoberta(XLMRobertaForSequenceClassification),
        ModernBert(ModernBertCrossEncoder),
    }

    /// Cross-encoder on candle: XLM-RoBERTa (e.g. `BAAI/bge-reranker-base`) or ModernBERT
    /// (e.g. `Alibaba-NLP/gte-reranker-modernbert-base`), chosen from the checkpoint's config.
    pub struct CandleReranker {
        model: Mutex<Arch>,
        tokenizer: Tokenizer,
        device: Device,
        pad_id: u32,
    }

    impl CandleReranker {
        pub fn load(
            hf_repo: &str,
            backend: Backend,
            cache_dir: &Path,
            max_tokens: usize,
        ) -> anyhow::Result<Self> {
            let device = match backend {
                Backend::CandleMetal => Device::new_metal(0)?,
                _ => Device::Cpu,
            };
            let api = ApiBuilder::new()
                .with_cache_dir(cache_dir.to_path_buf())
                .with_progress(false)
                .build()?;
            let repo = api.repo(Repo::new(hf_repo.to_string(), RepoType::Model));
            let mut tokenizer =
                Tokenizer::from_file(repo.get("tokenizer.json").context("tokenizer.json")?)
                    .map_err(anyhow::Error::msg)?;
            tokenizer
                .with_truncation(Some(TruncationParams {
                    max_length: max_tokens,
                    strategy: TruncationStrategy::OnlySecond,
                    ..Default::default()
                }))
                .map_err(anyhow::Error::msg)?;
            tokenizer.with_padding(None);
            let cfg_text = std::fs::read_to_string(repo.get("config.json")?)?;
            let raw: serde_json::Value = serde_json::from_str(&cfg_text)?;
            let weights = repo.get("model.safetensors").context("model.safetensors")?;
            let (model, pad_id) = if raw["model_type"] == "modernbert" {
                // SAFETY: hf-hub cache files are immutable while mapped.
                let dtype = if device.is_metal() {
                    DType::F16
                } else {
                    DType::F32
                };
                let vb =
                    unsafe { VarBuilder::from_mmaped_safetensors(&[weights], dtype, &device)? };
                let cfg: modernbert::Config = serde_json::from_str(&cfg_text)?;
                let pad = cfg.pad_token_id;
                (
                    Arch::ModernBert(ModernBertCrossEncoder::load(vb, &cfg, max_tokens + 8)?),
                    pad,
                )
            } else {
                // SAFETY: as above. candle's XLM-R builds its attention mask in f32.
                let vb = unsafe {
                    VarBuilder::from_mmaped_safetensors(&[weights], DType::F32, &device)?
                };
                let cfg: Config = serde_json::from_str(&cfg_text)?;
                (
                    Arch::XlmRoberta(XLMRobertaForSequenceClassification::new(1, &cfg, vb)?),
                    1,
                )
            };
            Ok(Self {
                model: Mutex::new(model),
                tokenizer,
                device,
                pad_id,
            })
        }
    }

    impl Reranker for CandleReranker {
        fn score(&self, query: &str, documents: &[&str]) -> anyhow::Result<Vec<f32>> {
            if documents.is_empty() {
                return Ok(Vec::new());
            }
            let inputs: Vec<EncodeInput> = documents.iter().map(|d| (query, *d).into()).collect();
            let enc = self
                .tokenizer
                .encode_batch(inputs, true)
                .map_err(anyhow::Error::msg)?;
            let len = enc.iter().map(|e| e.get_ids().len()).max().unwrap_or(1);
            let (mut ids, mut mask) = (Vec::new(), Vec::new());
            for e in &enc {
                let n = e.get_ids().len();
                ids.extend_from_slice(e.get_ids());
                ids.extend(std::iter::repeat_n(self.pad_id, len - n));
                mask.extend(std::iter::repeat_n(1u32, n));
                mask.extend(std::iter::repeat_n(0u32, len - n));
            }
            let shape = (enc.len(), len);
            let ids = Tensor::from_vec(ids, shape, &self.device)?;
            let mask = Tensor::from_vec(mask, shape, &self.device)?;
            let types = ids.zeros_like()?;
            let model = self
                .model
                .lock()
                .map_err(|_| anyhow::anyhow!("reranker lock poisoned"))?;
            let logits = match &*model {
                Arch::XlmRoberta(m) => m.forward(&ids, &mask, &types)?,
                Arch::ModernBert(m) => m.forward(&ids, &mask)?,
            };
            Ok(logits.squeeze(1)?.to_dtype(DType::F32)?.to_vec1::<f32>()?)
        }
    }
}
