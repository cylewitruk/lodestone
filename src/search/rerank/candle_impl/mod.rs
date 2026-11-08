use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, bail};
use candle_core::{D, DType, Device, Module, Tensor};
use candle_nn::{LayerNorm, Linear, VarBuilder, ops};
use candle_transformers::models::bert::{self, BertModel};
use candle_transformers::models::xlm_roberta::{self, XLMRobertaForSequenceClassification};
use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Repo, RepoType};
use serde_json::Value;
use tokenizers::{EncodeInput, Tokenizer, TruncationParams, TruncationStrategy};

mod decoder;
mod flash_xlmr;

use decoder::{JinaV3Reranker, Qwen3Reranker};
use flash_xlmr::FlashXlmrCrossEncoder;

use super::Reranker;
use crate::config::Backend;
use crate::embed::modernbert::{self, ModernBertEncoder};

/// ModernBERT cross-encoder: pool (first token or mean, per `classifier_pooling`) →
/// dense → activation (`classifier_activation`) → norm → one logit. candle's own
/// classification head ends in a softmax, which is constant for one logit, so the head
/// lives here.
pub struct ModernBertCrossEncoder {
    encoder: ModernBertEncoder,
    dense: Linear,
    norm: LayerNorm,
    classifier: Linear,
    mean_pool: bool,
    silu: bool,
}

impl ModernBertCrossEncoder {
    fn load(
        vb: VarBuilder,
        cfg: &modernbert::Config,
        raw: &Value,
        max_len: usize,
    ) -> candle_core::Result<Self> {
        let h = cfg.hidden_size;
        Ok(Self {
            encoder: ModernBertEncoder::load(vb.pp("model"), cfg, max_len)?,
            dense: candle_nn::linear_no_bias(h, h, vb.pp("head.dense"))?,
            norm: LayerNorm::new_no_bias(vb.get(h, "head.norm.weight")?, cfg.layer_norm_eps),
            classifier: candle_nn::linear(h, 1, vb.pp("classifier"))?,
            mean_pool: raw["classifier_pooling"] != "cls",
            silu: raw["classifier_activation"] == "silu",
        })
    }

    fn forward(&self, ids: &Tensor, mask: &Tensor) -> candle_core::Result<Tensor> {
        let hidden = self.encoder.forward(ids, mask)?.to_dtype(DType::F32)?;
        let pooled = if self.mean_pool {
            let m = mask.to_dtype(DType::F32)?.unsqueeze(D::Minus1)?;
            hidden
                .broadcast_mul(&m)?
                .sum(1)?
                .broadcast_div(&m.sum(1)?)?
        } else {
            hidden.narrow(1, 0, 1)?.squeeze(1)?.contiguous()?
        };
        let x = self
            .dense
            .forward(&pooled.to_dtype(self.dense.weight().dtype())?)?;
        let x = if self.silu {
            ops::silu(&x)?
        } else {
            x.gelu_erf()?
        };
        self.classifier.forward(&self.norm.forward(&x)?)
    }
}

/// BERT cross-encoder (`BertForSequenceClassification`, e.g. ms-marco-MiniLM): the
/// first token through the tanh pooler, then one logit.
pub struct BertCrossEncoder {
    bert: BertModel,
    pooler: Linear,
    classifier: Linear,
}

impl BertCrossEncoder {
    fn load(vb: VarBuilder, cfg: &bert::Config) -> candle_core::Result<Self> {
        let h = cfg.hidden_size;
        Ok(Self {
            bert: BertModel::load(vb.clone(), cfg)?,
            pooler: candle_nn::linear(h, h, vb.pp("bert.pooler.dense"))?,
            classifier: candle_nn::linear(h, 1, vb.pp("classifier"))?,
        })
    }

    fn forward(&self, ids: &Tensor, types: &Tensor, mask: &Tensor) -> candle_core::Result<Tensor> {
        let hidden = self.bert.forward(ids, types, Some(mask))?;
        // Metal matmul needs contiguous inputs; the first-token slice is strided.
        let first = hidden.narrow(1, 0, 1)?.squeeze(1)?.contiguous()?;
        self.classifier
            .forward(&self.pooler.forward(&first)?.tanh()?)
    }
}

enum Arch {
    XlmRoberta(XLMRobertaForSequenceClassification),
    FlashXlmr(FlashXlmrCrossEncoder),
    ModernBert(ModernBertCrossEncoder),
    Bert(BertCrossEncoder),
    Qwen3(Qwen3Reranker),
    JinaV3(JinaV3Reranker),
}

/// Which implementation a checkpoint needs, from its config.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    XlmRoberta,
    FlashXlmr,
    ModernBert,
    Bert,
    Qwen3,
    JinaV3,
}

impl Kind {
    fn of(config: &Value) -> Option<Self> {
        if config["architectures"][0] == "JinaForRanking" {
            return Some(Self::JinaV3);
        }
        // Some remote-code checkpoints (jina-reranker-v2) omit `model_type`; fall back to the
        // architecture class.
        let model_type = config["model_type"].as_str().or_else(|| {
            config["architectures"][0]
                .as_str()?
                .starts_with("XLMRoberta")
                .then_some("xlm-roberta")
        })?;
        Some(match model_type {
            "xlm-roberta" if !config["use_flash_attn"].is_null() => Self::FlashXlmr,
            "xlm-roberta" => Self::XlmRoberta,
            "modernbert" => Self::ModernBert,
            "bert" => Self::Bert,
            "qwen3" => Self::Qwen3,
            _ => return None,
        })
    }

    /// Prompt-building rerankers truncate documents themselves; pair encoders let the
    /// tokenizer truncate the document side.
    fn builds_prompt(self) -> bool {
        matches!(self, Self::Qwen3 | Self::JinaV3)
    }
}

/// Cross-encoder on candle, chosen from the checkpoint's config: XLM-RoBERTa (bge-reranker),
/// Jina's flash XLM-RoBERTa (jina-reranker-v2), ModernBERT (gte/granite rerankers), BERT
/// (ms-marco-MiniLM), Qwen3 (Qwen3-Reranker) or jina-reranker-v3.
pub struct CandleReranker {
    model: Mutex<Arch>,
    tokenizer: Tokenizer,
    device: Device,
    pad_id: u32,
    max_tokens: usize,
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
        tokenizer.with_padding(None);
        let cfg_text = std::fs::read_to_string(repo.get("config.json")?)?;
        let raw: Value = serde_json::from_str(&cfg_text)?;
        let weights = repo.get("model.safetensors").context("model.safetensors")?;
        let half = if device.is_metal() {
            DType::F16
        } else {
            DType::F32
        };
        let Some(kind) = Kind::of(&raw) else {
            bail!(
                "unsupported reranker architecture {:?} / {:?} in {hf_repo}",
                raw["model_type"],
                raw["architectures"]
            );
        };
        let dtype = match kind {
            // candle's BERT and XLM-R build attention masks in f32.
            Kind::Bert | Kind::XlmRoberta => DType::F32,
            _ => half,
        };
        // SAFETY: hf-hub cache files are immutable while mapped.
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights], dtype, &device)? };
        let (model, pad_id) = match kind {
            Kind::ModernBert => {
                let cfg: modernbert::Config = serde_json::from_str(&cfg_text)?;
                let pad = cfg.pad_token_id;
                let m = ModernBertCrossEncoder::load(vb, &cfg, &raw, max_tokens + 8)?;
                (Arch::ModernBert(m), pad)
            }
            Kind::Bert => {
                let cfg: bert::Config = serde_json::from_str(&cfg_text)?;
                (Arch::Bert(BertCrossEncoder::load(vb, &cfg)?), 0)
            }
            Kind::XlmRoberta => {
                let cfg: xlm_roberta::Config = serde_json::from_str(&cfg_text)?;
                let m = XLMRobertaForSequenceClassification::new(1, &cfg, vb)?;
                (Arch::XlmRoberta(m), 1)
            }
            Kind::FlashXlmr => {
                anyhow::ensure!(
                    FlashXlmrCrossEncoder::matches(&vb),
                    "{hf_repo}: expected fused Wqkv weights"
                );
                (Arch::FlashXlmr(FlashXlmrCrossEncoder::load(vb, &raw)?), 1)
            }
            Kind::Qwen3 => (Arch::Qwen3(Qwen3Reranker::load(vb, &raw, &tokenizer)?), 0),
            Kind::JinaV3 => (Arch::JinaV3(JinaV3Reranker::load(vb, &raw, &tokenizer)?), 0),
        };
        if !kind.builds_prompt() {
            tokenizer
                .with_truncation(Some(TruncationParams {
                    max_length: max_tokens,
                    strategy: TruncationStrategy::OnlySecond,
                    ..Default::default()
                }))
                .map_err(anyhow::Error::msg)?;
        }
        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            device,
            pad_id,
            max_tokens,
        })
    }
}

impl Reranker for CandleReranker {
    fn score(&self, query: &str, documents: &[&str]) -> anyhow::Result<Vec<f32>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let model = self
            .model
            .lock()
            .map_err(|_| anyhow::anyhow!("reranker lock poisoned"))?;
        let (tok, dev, max) = (&self.tokenizer, &self.device, self.max_tokens);
        match &*model {
            Arch::Qwen3(m) => return m.score(tok, dev, query, documents, max),
            Arch::JinaV3(m) => return m.score(tok, dev, query, documents, max),
            _ => {}
        }
        let inputs: Vec<EncodeInput> = documents.iter().map(|d| (query, *d).into()).collect();
        let enc = self
            .tokenizer
            .encode_batch(inputs, true)
            .map_err(anyhow::Error::msg)?;
        let len = enc.iter().map(|e| e.get_ids().len()).max().unwrap_or(1);
        let (mut ids, mut types, mut mask) = (Vec::new(), Vec::new(), Vec::new());
        for e in &enc {
            let n = e.get_ids().len();
            ids.extend_from_slice(e.get_ids());
            ids.extend(std::iter::repeat_n(self.pad_id, len - n));
            types.extend_from_slice(e.get_type_ids());
            types.extend(std::iter::repeat_n(0u32, len - n));
            mask.extend(std::iter::repeat_n(1u32, n));
            mask.extend(std::iter::repeat_n(0u32, len - n));
        }
        let shape = (enc.len(), len);
        let ids = Tensor::from_vec(ids, shape, &self.device)?;
        let types = Tensor::from_vec(types, shape, &self.device)?;
        let mask = Tensor::from_vec(mask, shape, &self.device)?;
        let logits = match &*model {
            Arch::XlmRoberta(m) => m.forward(&ids, &mask, &types.zeros_like()?)?,
            Arch::ModernBert(m) => m.forward(&ids, &mask)?,
            Arch::Bert(m) => m.forward(&ids, &types, &mask)?,
            Arch::FlashXlmr(m) => m.forward(&ids, &mask)?,
            Arch::Qwen3(_) | Arch::JinaV3(_) => unreachable!("prompt rerankers return above"),
        };
        Ok(logits.squeeze(1)?.to_dtype(DType::F32)?.to_vec1::<f32>()?)
    }
}
