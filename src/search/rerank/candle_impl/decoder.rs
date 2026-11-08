//! Rerankers built on a decoder-only transformer: Qwen3-Reranker (pointwise yes/no) and
//! jina-reranker-v3 (listwise: every candidate in one prompt).

use anyhow::Context;
use candle_core::{DType, Device, IndexOp, Module, Tensor};
use candle_nn::{Linear, VarBuilder, ops};
use serde_json::Value;
use tokenizers::Tokenizer;

use crate::config::CODE_SEARCH_TASK;
use crate::embed::decoder::Decoder;

fn token(tokenizer: &Tokenizer, t: &str) -> anyhow::Result<u32> {
    tokenizer
        .token_to_id(t)
        .with_context(|| format!("token {t:?} missing from tokenizer"))
}

fn encode(tokenizer: &Tokenizer, text: &str) -> anyhow::Result<Vec<u32>> {
    Ok(tokenizer
        .encode(text, false)
        .map_err(anyhow::Error::msg)?
        .get_ids()
        .to_vec())
}

/// Qwen3-Reranker: a chat prompt asking whether the document meets the query; the score is
/// P("yes") against P("no") at the last position.
pub struct Qwen3Reranker {
    decoder: Decoder,
    /// Rows of the (tied) output embedding for "no" and "yes": (2, hidden).
    no_yes: Tensor,
    prefix: Vec<u32>,
    suffix: Vec<u32>,
}

const QWEN3_SYSTEM: &str = "<|im_start|>system\nJudge whether the Document meets the requirements based on the Query and the Instruct provided. Note that the answer can only be \"yes\" or \"no\".<|im_end|>\n<|im_start|>user\n";
const QWEN3_SUFFIX: &str = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";

impl Qwen3Reranker {
    pub fn load(vb: VarBuilder, config: &Value, tokenizer: &Tokenizer) -> anyhow::Result<Self> {
        let decoder = Decoder::load(vb.clone(), config)?;
        let name = if vb.contains_tensor("model.embed_tokens.weight") {
            "model.embed_tokens.weight"
        } else {
            "embed_tokens.weight"
        };
        let embed = vb.get((decoder.vocab_size(), decoder.hidden_size()), name)?;
        let rows = Tensor::new(
            &[token(tokenizer, "no")?, token(tokenizer, "yes")?],
            embed.device(),
        )?;
        Ok(Self {
            no_yes: embed.index_select(&rows, 0)?,
            prefix: encode(tokenizer, QWEN3_SYSTEM)?,
            suffix: encode(tokenizer, QWEN3_SUFFIX)?,
            decoder,
        })
    }

    pub fn score(
        &self,
        tokenizer: &Tokenizer,
        device: &Device,
        query: &str,
        documents: &[&str],
        max_tokens: usize,
    ) -> anyhow::Result<Vec<f32>> {
        let head = encode(
            tokenizer,
            &format!("<Instruct>: {CODE_SEARCH_TASK}\n<Query>: {query}\n<Document>: "),
        )?;
        let rows: Vec<Vec<u32>> = documents
            .iter()
            .map(|d| -> anyhow::Result<Vec<u32>> {
                let doc = encode(tokenizer, d)?;
                let doc = &doc[..doc.len().min(max_tokens)];
                Ok([&self.prefix[..], &head, doc, &self.suffix[..]].concat())
            })
            .collect::<anyhow::Result<_>>()?;
        let lens: Vec<usize> = rows.iter().map(Vec::len).collect();
        let len = lens.iter().copied().max().unwrap_or(1);
        let flat: Vec<u32> = rows
            .iter()
            .flat_map(|r| {
                r.iter()
                    .copied()
                    .chain(std::iter::repeat_n(0, len - r.len()))
            })
            .collect();
        let ids = Tensor::from_vec(flat, (rows.len(), len), device)?;
        let last = self.decoder.last_hidden(&ids, &lens)?;
        let logits = last.matmul(&self.no_yes.t()?)?.to_dtype(DType::F32)?;
        let p = ops::softmax_last_dim(&logits)?;
        Ok(p.i((.., 1))?.to_vec1::<f32>()?)
    }
}

/// jina-reranker-v3: the query and every candidate go into one prompt, each candidate
/// followed by an embed token and the query by a rerank token. Their hidden states pass
/// through a two-layer projector; the score is the cosine between query and candidate.
pub struct JinaV3Reranker {
    decoder: Decoder,
    proj_in: Linear,
    proj_out: Linear,
    doc_token: u32,
    query_token: u32,
}

const JINA_V3_DOC_TOKEN: &str = "<|embed_token|>";
const JINA_V3_QUERY_TOKEN: &str = "<|rerank_token|>";
const JINA_V3_PROJECTION_DIM: usize = 512;
const JINA_V3_SYSTEM: &str = "<|im_start|>system\nYou are a search relevance expert who can determine a ranking of the passages based on how relevant they are to the query. If the query is a question, how relevant a passage is depends on how well it answers the question. If not, try to analyze the intent of the query and assess how well each passage satisfies the intent. If an instruction is provided, you should follow the instruction when determining the ranking.<|im_end|>\n<|im_start|>user\n";
const JINA_V3_SUFFIX: &str = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";

impl JinaV3Reranker {
    pub fn load(vb: VarBuilder, config: &Value, tokenizer: &Tokenizer) -> anyhow::Result<Self> {
        let decoder = Decoder::load(vb.clone(), config)?;
        let h = decoder.hidden_size();
        Ok(Self {
            proj_in: candle_nn::linear_no_bias(h, h / 2, vb.pp("projector.0"))?,
            proj_out: candle_nn::linear_no_bias(
                h / 2,
                JINA_V3_PROJECTION_DIM,
                vb.pp("projector.2"),
            )?,
            doc_token: token(tokenizer, JINA_V3_DOC_TOKEN)?,
            query_token: token(tokenizer, JINA_V3_QUERY_TOKEN)?,
            decoder,
        })
    }

    fn project(&self, x: &Tensor) -> candle_core::Result<Tensor> {
        self.proj_out.forward(&self.proj_in.forward(x)?.relu()?)
    }

    pub fn score(
        &self,
        tokenizer: &Tokenizer,
        device: &Device,
        query: &str,
        documents: &[&str],
        max_tokens: usize,
    ) -> anyhow::Result<Vec<f32>> {
        let clean = |t: &str| {
            t.replace(JINA_V3_DOC_TOKEN, "")
                .replace(JINA_V3_QUERY_TOKEN, "")
        };
        let query = clean(query);
        let mut prompt = format!(
            "{JINA_V3_SYSTEM}I will provide you with {} passages, each indicated by a numerical identifier. Rank the passages based on their relevance to query: {query}\n",
            documents.len()
        );
        for (i, d) in documents.iter().enumerate() {
            let ids = encode(tokenizer, &clean(d))?;
            let doc = tokenizer
                .decode(&ids[..ids.len().min(max_tokens)], false)
                .map_err(anyhow::Error::msg)?;
            prompt.push_str(&format!(
                "<passage id=\"{i}\">\n{doc}{JINA_V3_DOC_TOKEN}\n</passage>\n"
            ));
        }
        prompt.push_str(&format!(
            "<query>\n{query}{JINA_V3_QUERY_TOKEN}\n</query>{JINA_V3_SUFFIX}"
        ));
        let ids = encode(tokenizer, &prompt)?;
        let positions = |t: u32| -> Vec<usize> {
            ids.iter()
                .enumerate()
                .filter(|(_, id)| **id == t)
                .map(|(i, _)| i)
                .collect()
        };
        let (docs, queries) = (positions(self.doc_token), positions(self.query_token));
        anyhow::ensure!(
            docs.len() == documents.len() && queries.len() == 1,
            "jina-reranker-v3 prompt has {} embed and {} rerank tokens for {} documents",
            docs.len(),
            queries.len(),
            documents.len()
        );
        let n = ids.len();
        let hidden = self
            .decoder
            .hidden(&Tensor::from_vec(ids, (1, n), device)?)?
            .squeeze(0)?;
        let pick = |p: &[usize]| -> candle_core::Result<Tensor> {
            let idx = Tensor::new(
                p.iter().map(|&i| i as u32).collect::<Vec<_>>().as_slice(),
                device,
            )?;
            hidden.index_select(&idx, 0)
        };
        let d = self.project(&pick(&docs)?)?.to_dtype(DType::F32)?;
        let q = self.project(&pick(&queries)?)?.to_dtype(DType::F32)?;
        let dot = d.matmul(&q.t()?)?.squeeze(1)?;
        let norms = d
            .sqr()?
            .sum(1)?
            .sqrt()?
            .affine(q.sqr()?.sum_all()?.sqrt()?.to_scalar::<f32>()? as f64, 0.0)?;
        Ok((dot / norms)?.to_vec1::<f32>()?)
    }
}
