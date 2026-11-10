//! Building blocks shared by the bidirectional encoders written for lodestone (EmbeddingGemma 2,
//! UltraBERT): rotary tables, additive masks and grouped-query attention.

use candle_core::{D, DType, Device, Result, Tensor};
use candle_nn::ops;

/// Additive mask value; finite so fully masked rows stay finite in any dtype.
pub const MASKED: f32 = -10_000.0;

/// Rotary embedding table for one head size and base: `cos`/`sin` of shape (positions, dim/2),
/// as `candle_nn::rotary_emb::rope` expects (half-rotation layout).
pub struct Rope {
    cos: Tensor,
    sin: Tensor,
}

impl Rope {
    pub fn new(theta: f64, dim: usize, max_len: usize, dtype: DType, dev: &Device) -> Result<Self> {
        let inv: Vec<f32> = (0..dim)
            .step_by(2)
            .map(|i| (1.0 / theta.powf(i as f64 / dim as f64)) as f32)
            .collect();
        let n = inv.len();
        let inv = Tensor::from_vec(inv, (1, n), dev)?;
        let t = Tensor::arange(0u32, max_len as u32, dev)?
            .to_dtype(DType::F32)?
            .reshape((max_len, 1))?;
        let freqs = t.matmul(&inv)?;
        Ok(Self {
            cos: freqs.cos()?.to_dtype(dtype)?,
            sin: freqs.sin()?.to_dtype(dtype)?,
        })
    }

    /// Applies the rotation to `x`: (batch, heads, seq, dim).
    pub fn apply(&self, x: &Tensor) -> Result<Tensor> {
        let l = x.dim(2)?;
        candle_nn::rotary_emb::rope(
            &x.contiguous()?,
            &self.cos.narrow(0, 0, l)?,
            &self.sin.narrow(0, 0, l)?,
        )
    }
}

/// Padding mask (batch, seq) of 0/1 → additive (batch, 1, 1, seq).
pub fn padding_mask(mask: &Tensor, dtype: DType) -> Result<Tensor> {
    let (b, l) = mask.dims2()?;
    ((mask.to_dtype(DType::F32)? - 1.0)? * -(MASKED as f64))?
        .reshape((b, 1, 1, l))?
        .to_dtype(dtype)
}

/// Additive (seq, seq) mask allowing only |i − j| ≤ `window`.
pub fn window_mask(l: usize, window: usize, dtype: DType, dev: &Device) -> Result<Tensor> {
    let v: Vec<f32> = (0..l)
        .flat_map(|i| (0..l).map(move |j| if i.abs_diff(j) > window { MASKED } else { 0.0 }))
        .collect();
    Tensor::from_vec(v, (l, l), dev)?.to_dtype(dtype)
}

/// Scaled dot-product attention with grouped key/value heads. `q`: (b, h, l, d); `k`, `v`:
/// (b, kv, l, d); `mask` broadcasts to (b, h, l, l). Softmax runs in f32.
pub fn attention(q: &Tensor, k: &Tensor, v: &Tensor, mask: &Tensor, scale: f64) -> Result<Tensor> {
    let (b, h, l, d) = q.dims4()?;
    let kv = k.dim(1)?;
    let repeat = |t: &Tensor| -> Result<Tensor> {
        if kv == h {
            return Ok(t.clone());
        }
        t.unsqueeze(2)?
            .expand((b, kv, h / kv, l, d))?
            .reshape((b, h, l, d))
    };
    let (k, v) = (repeat(k)?, repeat(v)?);
    let dtype = q.dtype();
    let att = (q.contiguous()?.matmul(&k.t()?.contiguous()?)? * scale)?.broadcast_add(mask)?;
    let att = ops::softmax_last_dim(&att.to_dtype(DType::F32)?)?.to_dtype(dtype)?;
    att.matmul(&v.contiguous()?)
}

/// (b, l, h·d) → (b, h, l, d).
pub fn split_heads(x: &Tensor, heads: usize, head_dim: usize) -> Result<Tensor> {
    let (b, l, _) = x.dims3()?;
    x.reshape((b, l, heads, head_dim))?.transpose(1, 2)
}

/// (b, h, l, d) → (b, l, h·d).
pub fn merge_heads(x: &Tensor) -> Result<Tensor> {
    let (b, h, l, d) = x.dims4()?;
    x.transpose(1, 2)?.reshape((b, l, h * d))
}

/// RMS normalisation over the last dimension without a learned scale.
pub fn rms_unit(x: &Tensor, eps: f64) -> Result<Tensor> {
    let dtype = x.dtype();
    let x = x.to_dtype(DType::F32)?;
    let ms = x.sqr()?.mean_keepdim(D::Minus1)?;
    x.broadcast_div(&(ms + eps)?.sqrt()?)?.to_dtype(dtype)
}
