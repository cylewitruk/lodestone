//! Jina's flash-attention XLM-RoBERTa cross-encoder (jina-reranker-v2-base-multilingual):
//! standard post-norm XLM-RoBERTa layers stored under different weight names, with a fused
//! query/key/value projection, and the RoBERTa classification head (first token → dense →
//! tanh → one logit).

use candle_core::{D, DType, Module, Result, Tensor};
use candle_nn::{Embedding, LayerNorm, Linear, VarBuilder, embedding, layer_norm, linear, ops};
use serde_json::Value;

/// Additive mask value; finite so f16 stays finite.
const MASKED: f64 = -10_000.0;
/// XLM-RoBERTa's padding id; positions of real tokens start right after it.
const PADDING_IDX: u32 = 1;

struct Layer {
    qkv: Linear,
    out: Linear,
    norm1: LayerNorm,
    fc1: Linear,
    fc2: Linear,
    norm2: LayerNorm,
}

pub struct FlashXlmrCrossEncoder {
    word: Embedding,
    position: Embedding,
    token_type: Embedding,
    emb_norm: LayerNorm,
    layers: Vec<Layer>,
    dense: Linear,
    out_proj: Linear,
    heads: usize,
}

impl FlashXlmrCrossEncoder {
    /// Whether `vb` holds this layout (fused `mixer.Wqkv` weights).
    pub fn matches(vb: &VarBuilder) -> bool {
        vb.contains_tensor("roberta.encoder.layers.0.mixer.Wqkv.weight")
    }

    pub fn load(vb: VarBuilder, config: &Value) -> Result<Self> {
        let n = |k: &str| config[k].as_u64().unwrap_or(0) as usize;
        let (h, inter) = (n("hidden_size"), n("intermediate_size"));
        let eps = config["layer_norm_eps"].as_f64().unwrap_or(1e-5);
        let e = vb.pp("roberta.embeddings");
        let layers = (0..n("num_hidden_layers"))
            .map(|i| {
                let l = vb.pp(format!("roberta.encoder.layers.{i}"));
                Ok(Layer {
                    qkv: linear(h, 3 * h, l.pp("mixer.Wqkv"))?,
                    out: linear(h, h, l.pp("mixer.out_proj"))?,
                    norm1: layer_norm(h, eps, l.pp("norm1"))?,
                    fc1: linear(h, inter, l.pp("mlp.fc1"))?,
                    fc2: linear(inter, h, l.pp("mlp.fc2"))?,
                    norm2: layer_norm(h, eps, l.pp("norm2"))?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            word: embedding(n("vocab_size"), h, e.pp("word_embeddings"))?,
            position: embedding(n("max_position_embeddings"), h, e.pp("position_embeddings"))?,
            token_type: embedding(
                n("type_vocab_size").max(1),
                h,
                e.pp("token_type_embeddings"),
            )?,
            emb_norm: layer_norm(h, eps, vb.pp("roberta.emb_ln"))?,
            layers,
            dense: linear(h, h, vb.pp("classifier.dense"))?,
            out_proj: linear(h, 1, vb.pp("classifier.out_proj"))?,
            heads: n("num_attention_heads"),
        })
    }

    /// `ids`, `mask`: (batch, seq) u32, right-padded → (batch, 1) logits.
    pub fn forward(&self, ids: &Tensor, mask: &Tensor) -> Result<Tensor> {
        let (b, l) = ids.dims2()?;
        let dtype = self.emb_norm.weight().dtype();
        // RoBERTa positions: padding_idx + running count of real tokens; padding keeps
        // padding_idx.
        let m = mask.to_dtype(DType::F32)?;
        let positions = ((m.cumsum(1)? * &m)? + PADDING_IDX as f64)?.to_dtype(DType::U32)?;
        let x = (self.word.forward(ids)? + self.position.forward(&positions)?)?
            .broadcast_add(&self.token_type.forward(&ids.zeros_like()?)?)?;
        let mut x = self.emb_norm.forward(&x)?;
        let additive = ((m - 1.0)? * -MASKED)?
            .reshape((b, 1, 1, l))?
            .to_dtype(dtype)?;
        let d = x.dim(D::Minus1)?;
        let hd = d / self.heads;
        for layer in &self.layers {
            let qkv = layer
                .qkv
                .forward(&x)?
                .reshape((b, l, 3, self.heads, hd))?
                .permute((2, 0, 3, 1, 4))?;
            let (q, k, v) = (
                qkv.get(0)?.contiguous()?,
                qkv.get(1)?.contiguous()?,
                qkv.get(2)?.contiguous()?,
            );
            let att = (q.matmul(&k.t()?)? * (hd as f64).powf(-0.5))?.broadcast_add(&additive)?;
            let ctx = ops::softmax_last_dim(&att)?
                .matmul(&v)?
                .transpose(1, 2)?
                .reshape((b, l, d))?;
            x = layer.norm1.forward(&(layer.out.forward(&ctx)? + &x)?)?;
            let mlp = layer.fc2.forward(&layer.fc1.forward(&x)?.gelu_erf()?)?;
            x = layer.norm2.forward(&(mlp + &x)?)?;
        }
        let first = x.narrow(1, 0, 1)?.squeeze(1)?.contiguous()?;
        self.out_proj.forward(&self.dense.forward(&first)?.tanh()?)
    }
}
