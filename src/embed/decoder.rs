//! Decoder-only transformers (Qwen2, Qwen3) used as encoders: hidden states for every token,
//! or for each row's last real token. Serves last-token-pooled embedders (Qwen3-Embedding,
//! harrier-oss, jina-code-embeddings) and decoder rerankers (Qwen3-Reranker, jina-reranker-v3).
//! Rows are right-padded: under causal attention a token never attends to positions after it,
//! so padding cannot reach any real token and no padding mask is needed.

use candle_core::{IndexOp, Result, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::{qwen2, qwen3};
use serde_json::Value;

/// Never run directly: each forward pass works on a clone (tensors are shared), so the
/// model's key/value cache starts empty every time.
enum Model {
    Qwen2(qwen2::Model),
    Qwen3(qwen3::Model),
}

pub struct Decoder {
    model: Model,
    vocab_size: usize,
    hidden_size: usize,
}

impl Decoder {
    /// Loads a Qwen2 or Qwen3 checkpoint (by `model_type`), with or without the `model.`
    /// weight prefix of the causal-LM wrappers.
    pub fn load(vb: VarBuilder, config: &Value) -> candle_core::Result<Self> {
        let vb = if vb.contains_tensor("model.embed_tokens.weight") {
            vb
        } else {
            vb.rename_f(|name: &str| name.strip_prefix("model.").unwrap_or(name).to_string())
        };
        let field = |k: &str| config[k].as_u64().unwrap_or(0) as usize;
        let parse_err = |e: serde_json::Error| candle_core::Error::Msg(e.to_string());
        let model = match config["model_type"].as_str() {
            Some("qwen2") => {
                let mut c = config.clone();
                // candle wants a number; an absent window means attention over everything.
                if c["sliding_window"].is_null() {
                    c["sliding_window"] = c["max_position_embeddings"].clone();
                }
                let cfg: qwen2::Config = serde_json::from_value(c).map_err(parse_err)?;
                Model::Qwen2(qwen2::Model::new(&cfg, vb)?)
            }
            Some("qwen3") => {
                let cfg: qwen3::Config =
                    serde_json::from_value(config.clone()).map_err(parse_err)?;
                Model::Qwen3(qwen3::Model::new(&cfg, vb)?)
            }
            other => candle_core::bail!("unsupported decoder model_type {other:?}"),
        };
        Ok(Self {
            model,
            vocab_size: field("vocab_size"),
            hidden_size: field("hidden_size"),
        })
    }

    pub fn vocab_size(&self) -> usize {
        self.vocab_size
    }

    pub fn hidden_size(&self) -> usize {
        self.hidden_size
    }

    /// `ids`: (batch, seq), right-padded → (batch, seq, hidden).
    pub fn hidden(&self, ids: &Tensor) -> Result<Tensor> {
        match &self.model {
            Model::Qwen2(m) => m.clone().forward(ids, 0, None),
            Model::Qwen3(m) => m.clone().forward(ids, 0),
        }
    }

    /// Hidden state of each row's last real token; `lens` are the real lengths →
    /// (batch, hidden).
    pub fn last_hidden(&self, ids: &Tensor, lens: &[usize]) -> Result<Tensor> {
        let hidden = self.hidden(ids)?;
        let rows = lens
            .iter()
            .enumerate()
            .map(|(i, &n)| hidden.i((i, n.max(1) - 1)))
            .collect::<Result<Vec<_>>>()?;
        Tensor::stack(&rows, 0)
    }
}
