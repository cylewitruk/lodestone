//! EmbeddingGemma 2 text encoder (`google/embeddinggemma-2`, the `language_model.*` weights):
//! bidirectional Gemma layers with q/k/v RMS norms, grouped-query attention whose shape differs
//! between sliding and full layers, a per-layer-input (PLE) gate after every block, and a final
//! projection to the embedding size. Mean pooling happens in the embedder.
//!
//! The sliding window (512 tokens) never binds at lodestone's chunk lengths, so sliding layers
//! attend over the whole (unpadded) sequence; `load` rejects a `max_len` that would make it bind.

use anyhow::{Context, bail};
use candle_core::{Module, Result, Tensor};
use candle_nn::{Embedding, Linear, RmsNorm, VarBuilder, embedding, linear_no_bias, rms_norm};
use serde_json::Value;

use super::layers::{Rope, attention, merge_heads, padding_mask, rms_unit, split_heads};

struct Layer {
    q: Linear,
    k: Linear,
    v: Linear,
    o: Linear,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    heads: usize,
    kv_heads: usize,
    head_dim: usize,
    full: bool,
    input_norm: RmsNorm,
    post_attn_norm: RmsNorm,
    pre_ff_norm: RmsNorm,
    post_ff_norm: RmsNorm,
    gate: Linear,
    up: Linear,
    down: Linear,
    ple_gate: Linear,
    ple_proj: Linear,
    ple_norm: RmsNorm,
    scalar: Tensor,
    eps: f64,
}

impl Layer {
    fn forward(&self, x: &Tensor, ple: &Tensor, mask: &Tensor, rope: &Rope) -> Result<Tensor> {
        let h = self.input_norm.forward(x)?;
        let q = self.q_norm.forward(&split_heads(
            &self.q.forward(&h)?,
            self.heads,
            self.head_dim,
        )?)?;
        let k = self.k_norm.forward(&split_heads(
            &self.k.forward(&h)?,
            self.kv_heads,
            self.head_dim,
        )?)?;
        let v = rms_unit(
            &split_heads(&self.v.forward(&h)?, self.kv_heads, self.head_dim)?,
            self.eps,
        )?;
        let (q, k) = (rope.apply(&q)?, rope.apply(&k)?);
        // EmbeddingGemma 2 attends without 1/sqrt(d) scaling (q/k are RMS-normalised).
        let ctx = merge_heads(&attention(&q, &k, &v, mask, 1.0)?)?;
        let x = (x + self.post_attn_norm.forward(&self.o.forward(&ctx)?)?)?;
        let h = self.pre_ff_norm.forward(&x)?;
        let ff = self
            .down
            .forward(&(self.gate.forward(&h)?.gelu()? * self.up.forward(&h)?)?)?;
        let x = (&x + self.post_ff_norm.forward(&ff)?)?;
        let g = (self.ple_gate.forward(&x)?.gelu()? * ple)?;
        let x = (&x + self.ple_norm.forward(&self.ple_proj.forward(&g)?)?)?;
        x.broadcast_mul(&self.scalar)
    }
}

pub struct EmbeddingGemma2 {
    embed: Embedding,
    embed_scale: f64,
    ple_projection: Linear,
    ple_norm: RmsNorm,
    layers: Vec<Layer>,
    norm: RmsNorm,
    projection: Linear,
    sliding_rope: Rope,
    full_rope: Rope,
    ple_dim: usize,
    hidden: usize,
}

impl EmbeddingGemma2 {
    /// `config` is the checkpoint's `config.json`; `max_len` bounds the input length.
    pub fn load(vb: VarBuilder, config: &Value, max_len: usize) -> anyhow::Result<Self> {
        let t = &config["text_config"];
        let n = |k: &str| -> anyhow::Result<usize> {
            t[k].as_u64()
                .map(|v| v as usize)
                .with_context(|| format!("text_config.{k} missing"))
        };
        let (hidden, layers_n, heads) = (
            n("hidden_size")?,
            n("num_hidden_layers")?,
            n("num_attention_heads")?,
        );
        let (inter, ple_dim, embed_dim) = (
            n("intermediate_size")?,
            n("hidden_size_per_layer_input")?,
            n("embedding_dim")?,
        );
        let (head_dim, kv_heads) = (n("head_dim")?, n("num_key_value_heads")?);
        let eps = t["rms_norm_eps"].as_f64().unwrap_or(1e-6);
        if let Some(window) = t["sliding_window"].as_u64()
            && max_len > window as usize
        {
            bail!("max_tokens {max_len} exceeds the sliding window {window}; not supported");
        }
        let theta = |kind: &str| {
            t["rope_parameters"][kind]["rope_theta"]
                .as_f64()
                .unwrap_or(10_000.0)
        };
        let types: Vec<bool> = t["layer_types"]
            .as_array()
            .context("text_config.layer_types missing")?
            .iter()
            .map(|v| v == "full_attention")
            .collect();
        let full_head_dim = t["per_layer_config"]
            .as_object()
            .and_then(|m| m.values().find_map(|c| c["head_dim"].as_u64()))
            .map_or(head_dim, |v| v as usize);
        let vb = vb.pp("language_model");
        let dev = vb.device().clone();
        let dtype = vb.dtype();
        let layers = (0..layers_n)
            .map(|i| -> anyhow::Result<Layer> {
                let per = &t["per_layer_config"][format!("{i:02}")];
                let hd = per["head_dim"].as_u64().map_or(head_dim, |v| v as usize);
                let kv = per["num_key_value_heads"]
                    .as_u64()
                    .map_or(kv_heads, |v| v as usize);
                let l = vb.pp(format!("layers.{i}"));
                let a = l.pp("self_attn");
                let p = l.pp("ple_block");
                Ok(Layer {
                    q: linear_no_bias(hidden, heads * hd, a.pp("q_proj"))?,
                    k: linear_no_bias(hidden, kv * hd, a.pp("k_proj"))?,
                    v: linear_no_bias(hidden, kv * hd, a.pp("v_proj"))?,
                    o: linear_no_bias(heads * hd, hidden, a.pp("o_proj"))?,
                    q_norm: rms_norm(hd, eps, a.pp("q_norm"))?,
                    k_norm: rms_norm(hd, eps, a.pp("k_norm"))?,
                    heads,
                    kv_heads: kv,
                    head_dim: hd,
                    full: types.get(i).copied().unwrap_or(false),
                    input_norm: rms_norm(hidden, eps, l.pp("input_layernorm"))?,
                    post_attn_norm: rms_norm(hidden, eps, l.pp("post_attention_layernorm"))?,
                    pre_ff_norm: rms_norm(hidden, eps, l.pp("pre_feedforward_layernorm"))?,
                    post_ff_norm: rms_norm(hidden, eps, l.pp("post_feedforward_layernorm"))?,
                    gate: linear_no_bias(hidden, inter, l.pp("mlp.gate_proj"))?,
                    up: linear_no_bias(hidden, inter, l.pp("mlp.up_proj"))?,
                    down: linear_no_bias(inter, hidden, l.pp("mlp.down_proj"))?,
                    ple_gate: linear_no_bias(hidden, ple_dim, p.pp("per_layer_input_gate"))?,
                    ple_proj: linear_no_bias(ple_dim, hidden, p.pp("per_layer_projection"))?,
                    ple_norm: rms_norm(hidden, eps, p.pp("post_per_layer_input_norm"))?,
                    scalar: l.get(1, "layer_scalar")?,
                    eps,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            embed: embedding(n("vocab_size")?, hidden, vb.pp("embed_tokens"))?,
            embed_scale: (hidden as f64).sqrt(),
            ple_projection: linear_no_bias(
                hidden,
                layers_n * ple_dim,
                vb.pp("ple.per_layer_model_projection"),
            )?,
            ple_norm: rms_norm(ple_dim, eps, vb.pp("ple.per_layer_projection_norm"))?,
            layers,
            norm: rms_norm(hidden, eps, vb.pp("norm"))?,
            projection: linear_no_bias(hidden, embed_dim, vb.pp("embedding_projection"))?,
            sliding_rope: Rope::new(theta("sliding_attention"), head_dim, max_len, dtype, &dev)?,
            full_rope: Rope::new(theta("full_attention"), full_head_dim, max_len, dtype, &dev)?,
            ple_dim,
            hidden,
        })
    }

    /// `ids`, `mask`: (batch, seq) u32 → (batch, seq, embedding_dim) before pooling.
    pub fn forward(&self, ids: &Tensor, mask: &Tensor) -> Result<Tensor> {
        let (b, l) = ids.dims2()?;
        let x = (self.embed.forward(ids)? * self.embed_scale)?;
        let ple = (self.ple_projection.forward(&x)? * (self.hidden as f64).powf(-0.5))?
            .reshape((b, l, self.layers.len(), self.ple_dim))?;
        let ple = self.ple_norm.forward(&ple)?;
        let mask = padding_mask(mask, x.dtype())?;
        let mut x = x;
        for (i, layer) in self.layers.iter().enumerate() {
            let rope = if layer.full {
                &self.full_rope
            } else {
                &self.sliding_rope
            };
            let ple_i = ple.narrow(2, i, 1)?.squeeze(2)?.contiguous()?;
            x = layer.forward(&x, &ple_i, &mask, rope)?;
        }
        self.projection.forward(&self.norm.forward(&x)?)
    }
}
