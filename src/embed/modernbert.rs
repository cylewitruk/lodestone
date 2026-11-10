//! ModernBERT encoder adapted from candle-transformers so it runs in f16 on Metal: candle's
//! version builds its attention masks in f32 (forcing f32 weights) and uses `-inf` in the
//! sliding-window mask, which turns fully masked padding rows into NaN under mean pooling.

use std::sync::Arc;

use candle_core::{D, DType, Device, Module, Result, Tensor};
use candle_nn::{
    Embedding, LayerNorm, Linear, VarBuilder, embedding, layer_norm_no_bias, linear_no_bias, ops,
};
pub use candle_transformers::models::modernbert::Config;

/// Additive mask value; finite so fully masked rows stay finite.
const MASKED: f64 = -10_000.0;

struct Rotary {
    sin: Tensor,
    cos: Tensor,
}

impl Rotary {
    fn new(dtype: DType, cfg: &Config, theta: f64, max_len: usize, dev: &Device) -> Result<Self> {
        let dim = cfg.hidden_size / cfg.num_attention_heads;
        let inv: Vec<f32> = (0..dim)
            .step_by(2)
            .map(|i| 1f32 / theta.powf(i as f64 / dim as f64) as f32)
            .collect();
        let n = inv.len();
        let inv = Tensor::from_vec(inv, (1, n), dev)?;
        let t = Tensor::arange(0u32, max_len as u32, dev)?
            .to_dtype(DType::F32)?
            .reshape((max_len, 1))?;
        let freqs = t.matmul(&inv)?;
        Ok(Self {
            sin: freqs.sin()?.to_dtype(dtype)?,
            cos: freqs.cos()?.to_dtype(dtype)?,
        })
    }
}

struct Layer {
    qkv: Linear,
    proj: Linear,
    wi: Linear,
    wo: Linear,
    attn_norm: Option<LayerNorm>,
    mlp_norm: LayerNorm,
    heads: usize,
    head_dim: usize,
    local: bool,
    rotary: Arc<Rotary>,
}

impl Layer {
    fn load(vb: VarBuilder, cfg: &Config, rotary: Arc<Rotary>, local: bool) -> Result<Self> {
        let h = cfg.hidden_size;
        Ok(Self {
            qkv: linear_no_bias(h, h * 3, vb.pp("attn.Wqkv"))?,
            proj: linear_no_bias(h, h, vb.pp("attn.Wo"))?,
            wi: linear_no_bias(h, cfg.intermediate_size * 2, vb.pp("mlp.Wi"))?,
            wo: linear_no_bias(cfg.intermediate_size, h, vb.pp("mlp.Wo"))?,
            attn_norm: layer_norm_no_bias(h, cfg.layer_norm_eps, vb.pp("attn_norm")).ok(),
            mlp_norm: layer_norm_no_bias(h, cfg.layer_norm_eps, vb.pp("mlp_norm"))?,
            heads: cfg.num_attention_heads,
            head_dim: h / cfg.num_attention_heads,
            local,
            rotary,
        })
    }

    fn forward(&self, xs: &Tensor, global: &Tensor, local: &Tensor) -> Result<Tensor> {
        let (b, l, d) = xs.dims3()?;
        let normed = match &self.attn_norm {
            Some(n) => n.forward(xs)?,
            None => xs.clone(),
        };
        let qkv = self
            .qkv
            .forward(&normed)?
            .reshape((b, l, 3, self.heads, self.head_dim))?
            .permute((2, 0, 3, 1, 4))?;
        let cos = self.rotary.cos.narrow(0, 0, l)?;
        let sin = self.rotary.sin.narrow(0, 0, l)?;
        let q = candle_nn::rotary_emb::rope(&qkv.get(0)?.contiguous()?, &cos, &sin)?;
        let k = candle_nn::rotary_emb::rope(&qkv.get(1)?.contiguous()?, &cos, &sin)?;
        let v = qkv.get(2)?.contiguous()?;
        let att =
            (q * (self.head_dim as f64).powf(-0.5))?.matmul(&k.transpose(D::Minus2, D::Minus1)?)?;
        let att = att.broadcast_add(global)?;
        let att = if self.local {
            att.broadcast_add(local)?
        } else {
            att
        };
        let att = ops::softmax_last_dim(&att)?;
        let ctx = att.matmul(&v)?.transpose(1, 2)?.reshape((b, l, d))?;
        let xs = (self.proj.forward(&ctx)? + xs)?;
        let h = self.wi.forward(&self.mlp_norm.forward(&xs)?)?;
        let parts = h.chunk(2, D::Minus1)?;
        let mlp = self.wo.forward(&(parts[0].gelu_erf()? * &parts[1])?)?;
        xs + mlp
    }
}

pub struct ModernBertEncoder {
    embeddings: Embedding,
    emb_norm: LayerNorm,
    layers: Vec<Layer>,
    final_norm: LayerNorm,
    window: usize,
    dtype: DType,
}

impl ModernBertEncoder {
    /// `vb` must address the backbone (`embeddings.*`, `layers.*`, `final_norm`).
    pub fn load(vb: VarBuilder, cfg: &Config, max_len: usize) -> Result<Self> {
        let dev = vb.device().clone();
        let dtype = vb.dtype();
        let global = Arc::new(Rotary::new(
            dtype,
            cfg,
            cfg.global_rope_theta,
            max_len,
            &dev,
        )?);
        let local = Arc::new(Rotary::new(
            dtype,
            cfg,
            cfg.local_rope_theta,
            max_len,
            &dev,
        )?);
        let layers = (0..cfg.num_hidden_layers)
            .map(|i| {
                let is_local = i % cfg.global_attn_every_n_layers != 0;
                Layer::load(
                    vb.pp(format!("layers.{i}")),
                    cfg,
                    if is_local {
                        local.clone()
                    } else {
                        global.clone()
                    },
                    is_local,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            embeddings: embedding(
                cfg.vocab_size,
                cfg.hidden_size,
                vb.pp("embeddings.tok_embeddings"),
            )?,
            emb_norm: layer_norm_no_bias(
                cfg.hidden_size,
                cfg.layer_norm_eps,
                vb.pp("embeddings.norm"),
            )?,
            layers,
            final_norm: layer_norm_no_bias(
                cfg.hidden_size,
                cfg.layer_norm_eps,
                vb.pp("final_norm"),
            )?,
            window: cfg.local_attention / 2,
            dtype,
        })
    }

    /// `ids`, `mask`: (batch, seq) u32 → (batch, seq, hidden).
    pub fn forward(&self, ids: &Tensor, mask: &Tensor) -> Result<Tensor> {
        let (_, l) = ids.dims2()?;
        let dev = ids.device();
        let global = ((mask.to_dtype(DType::F32)? - 1.0)? * -MASKED)?
            .unsqueeze(1)?
            .unsqueeze(1)?
            .to_dtype(self.dtype)?;
        let local: Vec<f32> = (0..l)
            .flat_map(|i| {
                (0..l).map(move |j| {
                    if i.abs_diff(j) > self.window {
                        MASKED as f32
                    } else {
                        0.0
                    }
                })
            })
            .collect();
        let local = Tensor::from_vec(local, (l, l), dev)?.to_dtype(self.dtype)?;
        let mut xs = self.emb_norm.forward(&self.embeddings.forward(ids)?)?;
        for layer in &self.layers {
            xs = layer.forward(&xs, &global, &local)?;
        }
        self.final_norm.forward(&xs)
    }
}
