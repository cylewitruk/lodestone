//! JinaBERT v2 "qk-post-norm" encoder (used by `jina-embeddings-v2-base-code`) with attention
//! masking. candle-transformers' `jina_bert` implements the older layout without q/k layer
//! norms or padding masks, so it cannot run this checkpoint in batches.

use candle_core::{D, DType, Device, Module, Result, Tensor};
use candle_nn::{
    Embedding, LayerNorm, Linear, VarBuilder, embedding, layer_norm, linear, linear_no_bias, ops,
};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct JinaConfig {
    pub vocab_size: usize,
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub intermediate_size: usize,
    pub type_vocab_size: usize,
    pub layer_norm_eps: f64,
}

struct Layer {
    query: Linear,
    key: Linear,
    value: Linear,
    ln_q: LayerNorm,
    ln_k: LayerNorm,
    out_dense: Linear,
    out_ln: LayerNorm,
    ln_1: LayerNorm,
    ln_2: LayerNorm,
    up_gated: Linear,
    down: Linear,
    heads: usize,
    head_dim: usize,
    intermediate: usize,
}

impl Layer {
    fn new(vb: VarBuilder, cfg: &JinaConfig) -> Result<Self> {
        let h = cfg.hidden_size;
        let eps = cfg.layer_norm_eps;
        let att = vb.pp("attention");
        let s = att.pp("self");
        Ok(Self {
            query: linear(h, h, s.pp("query"))?,
            key: linear(h, h, s.pp("key"))?,
            value: linear(h, h, s.pp("value"))?,
            ln_q: layer_norm(h, eps, s.pp("layer_norm_q"))?,
            ln_k: layer_norm(h, eps, s.pp("layer_norm_k"))?,
            out_dense: linear(h, h, att.pp("output").pp("dense"))?,
            out_ln: layer_norm(h, eps, att.pp("output").pp("LayerNorm"))?,
            ln_1: layer_norm(h, eps, vb.pp("layer_norm_1"))?,
            ln_2: layer_norm(h, eps, vb.pp("layer_norm_2"))?,
            up_gated: linear_no_bias(
                h,
                cfg.intermediate_size * 2,
                vb.pp("mlp").pp("up_gated_layer"),
            )?,
            down: linear(cfg.intermediate_size, h, vb.pp("mlp").pp("down_layer"))?,
            heads: cfg.num_attention_heads,
            head_dim: h / cfg.num_attention_heads,
            intermediate: cfg.intermediate_size,
        })
    }

    fn split_heads(&self, xs: &Tensor) -> Result<Tensor> {
        let (b, l, _) = xs.dims3()?;
        xs.reshape((b, l, self.heads, self.head_dim))?
            .transpose(1, 2)?
            .contiguous()
    }

    fn forward(&self, xs: &Tensor, bias: &Tensor) -> Result<Tensor> {
        let q = self.split_heads(&self.ln_q.forward(&self.query.forward(xs)?)?)?;
        let k = self.split_heads(&self.ln_k.forward(&self.key.forward(xs)?)?)?;
        let v = self.split_heads(&self.value.forward(xs)?)?;
        let scores = (q.matmul(&k.t()?)? / (self.head_dim as f64).sqrt())?;
        let probs = ops::softmax_last_dim(&scores.broadcast_add(bias)?)?;
        let ctx = probs
            .matmul(&v)?
            .transpose(1, 2)?
            .contiguous()?
            .flatten_from(D::Minus2)?;
        let attn = self
            .out_ln
            .forward(&(self.out_dense.forward(&ctx)? + xs)?)?;
        let residual = self.ln_1.forward(&(xs + attn)?)?;
        let h = self.up_gated.forward(&residual)?;
        let up = h.narrow(D::Minus1, 0, self.intermediate)?;
        let gated = h.narrow(D::Minus1, self.intermediate, self.intermediate)?;
        let mlp = self.down.forward(&(up * gated.gelu_erf()?)?)?;
        self.ln_2.forward(&(residual + mlp)?)
    }
}

pub struct JinaBert {
    word: Embedding,
    token_type: Embedding,
    emb_ln: LayerNorm,
    layers: Vec<Layer>,
    /// ALiBi bias, (1, heads, max_len, max_len).
    alibi: Tensor,
    dtype: DType,
}

impl JinaBert {
    pub fn new(vb: VarBuilder, cfg: &JinaConfig, max_len: usize) -> Result<Self> {
        let e = vb.pp("embeddings");
        let layers = (0..cfg.num_hidden_layers)
            .map(|i| Layer::new(vb.pp(format!("encoder.layer.{i}")), cfg))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            word: embedding(cfg.vocab_size, cfg.hidden_size, e.pp("word_embeddings"))?,
            token_type: embedding(
                cfg.type_vocab_size,
                cfg.hidden_size,
                e.pp("token_type_embeddings"),
            )?,
            emb_ln: layer_norm(cfg.hidden_size, cfg.layer_norm_eps, e.pp("LayerNorm"))?,
            layers,
            alibi: alibi_bias(cfg.num_attention_heads, max_len)?
                .to_dtype(vb.dtype())?
                .to_device(vb.device())?,
            dtype: vb.dtype(),
        })
    }

    /// `input_ids` and `mask` are (batch, seq) u32; returns (batch, seq, hidden).
    pub fn forward(&self, input_ids: &Tensor, mask: &Tensor) -> Result<Tensor> {
        let (_, seq) = input_ids.dims2()?;
        let types = input_ids.zeros_like()?;
        let xs = (self.word.forward(input_ids)? + self.token_type.forward(&types)?)?;
        let mut xs = self.emb_ln.forward(&xs)?;
        let alibi = self.alibi.narrow(2, 0, seq)?.narrow(3, 0, seq)?;
        let bias = alibi.broadcast_add(&extended_mask(mask, self.dtype)?)?;
        for layer in &self.layers {
            xs = layer.forward(&xs, &bias)?;
        }
        Ok(xs)
    }
}

/// (batch, seq) 1/0 mask → (batch, 1, 1, seq) additive mask.
pub fn extended_mask(mask: &Tensor, dtype: DType) -> Result<Tensor> {
    let m = mask.to_dtype(DType::F32)?;
    ((m - 1.0)? * 10_000.0)?
        .unsqueeze(1)?
        .unsqueeze(1)?
        .to_dtype(dtype)
}

fn alibi_slopes(n: usize) -> Vec<f32> {
    fn pow2(n: usize) -> Vec<f32> {
        let start = 2f32.powf(-(2f32.powf(-((n as f32).log2() - 3.0))));
        (0..n).map(|i| start * start.powi(i as i32)).collect()
    }
    if n.is_power_of_two() {
        pow2(n)
    } else {
        let closest = 1usize << (usize::BITS - 1 - n.leading_zeros());
        let mut s = pow2(closest);
        s.extend(
            alibi_slopes(2 * closest)
                .into_iter()
                .step_by(2)
                .take(n - closest),
        );
        s
    }
}

fn alibi_bias(heads: usize, size: usize) -> Result<Tensor> {
    let slopes = alibi_slopes(heads);
    let mut data = Vec::with_capacity(heads * size * size);
    for s in &slopes {
        for i in 0..size {
            for j in 0..size {
                data.push(-s * (i as f32 - j as f32).abs());
            }
        }
    }
    Tensor::from_vec(data, (1, heads, size, size), &Device::Cpu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slopes_match_reference_for_12_heads() {
        let s = alibi_slopes(12);
        assert_eq!(s.len(), 12);
        // Reference: get_slopes_power_of_2(8) + get_slopes_power_of_2(16)[0::2][:4]
        assert!((s[0] - 0.5).abs() < 1e-6);
        assert!((s[7] - 0.00390625).abs() < 1e-7);
        assert!((s[8] - 0.70710677).abs() < 1e-6);
        assert!((s[9] - 0.35355338).abs() < 1e-6);
    }
}
