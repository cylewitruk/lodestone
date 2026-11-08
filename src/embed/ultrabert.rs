//! UltraBERT encoder (`dmis-lab/UltraBERT-Embed-150M`): bidirectional Llama-style layers
//! (pre-RMS-norm, SwiGLU MLP) where each layer has its own rotary base and either a local window
//! (|i − j| ≤ w) or global attention, per `attention_windows` / `rope_theta` in the config.
//! First-token pooling happens in the embedder.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use anyhow::Context;
use candle_core::{Module, Result, Tensor};
use candle_nn::{Embedding, Linear, RmsNorm, VarBuilder, embedding, linear_no_bias, rms_norm};
use serde_json::Value;

use super::layers::{Rope, attention, merge_heads, padding_mask, split_heads, window_mask};

struct Layer {
    q: Linear,
    k: Linear,
    v: Linear,
    o: Linear,
    input_norm: RmsNorm,
    post_attn_norm: RmsNorm,
    gate: Linear,
    up: Linear,
    down: Linear,
    window: Option<usize>,
    /// Key into the rotary tables (the base's bit pattern).
    rope: u64,
}

pub struct UltraBert {
    embed: Embedding,
    layers: Vec<Layer>,
    norm: RmsNorm,
    ropes: BTreeMap<u64, Rope>,
    heads: usize,
    kv_heads: usize,
    head_dim: usize,
}

impl UltraBert {
    pub fn load(vb: VarBuilder, config: &Value, max_len: usize) -> anyhow::Result<Self> {
        let n = |k: &str| -> anyhow::Result<usize> {
            config[k]
                .as_u64()
                .map(|v| v as usize)
                .with_context(|| format!("config.{k} missing"))
        };
        let (hidden, layers_n, heads) = (
            n("hidden_size")?,
            n("num_hidden_layers")?,
            n("num_attention_heads")?,
        );
        let kv_heads = n("num_key_value_heads").unwrap_or(heads);
        let head_dim = n("head_dim").unwrap_or(hidden / heads);
        let inter = n("intermediate_size")?;
        let eps = config["rms_norm_eps"].as_f64().unwrap_or(1e-6);
        let thetas: Vec<f64> = match &config["rope_theta"] {
            Value::Array(a) => a.iter().map(|v| v.as_f64().unwrap_or(10_000.0)).collect(),
            v => vec![v.as_f64().unwrap_or(10_000.0); layers_n],
        };
        let windows: Vec<Option<usize>> = config["attention_windows"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_u64().map(|w| w as usize)).collect())
            .unwrap_or_else(|| vec![None; layers_n]);
        let dev = vb.device().clone();
        let dtype = vb.dtype();
        let mut ropes = BTreeMap::new();
        for &t in &thetas {
            if let Entry::Vacant(e) = ropes.entry(t.to_bits()) {
                e.insert(Rope::new(t, head_dim, max_len, dtype, &dev)?);
            }
        }
        let layers = (0..layers_n)
            .map(|i| -> anyhow::Result<Layer> {
                let l = vb.pp(format!("layers.{i}"));
                let a = l.pp("self_attn");
                Ok(Layer {
                    q: linear_no_bias(hidden, heads * head_dim, a.pp("q_proj"))?,
                    k: linear_no_bias(hidden, kv_heads * head_dim, a.pp("k_proj"))?,
                    v: linear_no_bias(hidden, kv_heads * head_dim, a.pp("v_proj"))?,
                    o: linear_no_bias(heads * head_dim, hidden, a.pp("o_proj"))?,
                    input_norm: rms_norm(hidden, eps, l.pp("input_layernorm"))?,
                    post_attn_norm: rms_norm(hidden, eps, l.pp("post_attention_layernorm"))?,
                    gate: linear_no_bias(hidden, inter, l.pp("mlp.gate_proj"))?,
                    up: linear_no_bias(hidden, inter, l.pp("mlp.up_proj"))?,
                    down: linear_no_bias(inter, hidden, l.pp("mlp.down_proj"))?,
                    window: windows.get(i).copied().flatten(),
                    rope: thetas.get(i).copied().unwrap_or(10_000.0).to_bits(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            embed: embedding(n("vocab_size")?, hidden, vb.pp("embed_tokens"))?,
            layers,
            norm: rms_norm(hidden, eps, vb.pp("norm"))?,
            ropes,
            heads,
            kv_heads,
            head_dim,
        })
    }

    /// `ids`, `mask`: (batch, seq) u32 → (batch, seq, hidden).
    pub fn forward(&self, ids: &Tensor, mask: &Tensor) -> Result<Tensor> {
        let l = ids.dim(1)?;
        let mut x = self.embed.forward(ids)?;
        let dtype = x.dtype();
        let pad = padding_mask(mask, dtype)?;
        let mut windowed: BTreeMap<usize, Tensor> = BTreeMap::new();
        for layer in &self.layers {
            let mask = match layer.window {
                Some(w) => match windowed.entry(w) {
                    Entry::Occupied(e) => e.get().clone(),
                    Entry::Vacant(e) => {
                        let local = window_mask(l, w, dtype, ids.device())?;
                        e.insert(pad.broadcast_add(&local)?).clone()
                    }
                },
                None => pad.clone(),
            };
            let rope = &self.ropes[&layer.rope];
            let h = layer.input_norm.forward(&x)?;
            let q = rope.apply(&split_heads(
                &layer.q.forward(&h)?,
                self.heads,
                self.head_dim,
            )?)?;
            let k = rope.apply(&split_heads(
                &layer.k.forward(&h)?,
                self.kv_heads,
                self.head_dim,
            )?)?;
            let v = split_heads(&layer.v.forward(&h)?, self.kv_heads, self.head_dim)?;
            let ctx = attention(&q, &k, &v, &mask, (self.head_dim as f64).powf(-0.5))?;
            x = (x + layer.o.forward(&merge_heads(&ctx)?)?)?;
            let h = layer.post_attn_norm.forward(&x)?;
            let ff = layer.down.forward(
                &(candle_nn::ops::silu(&layer.gate.forward(&h)?)? * layer.up.forward(&h)?)?,
            )?;
            x = (x + ff)?;
        }
        self.norm.forward(&x)
    }
}
