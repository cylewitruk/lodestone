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
pub mod candle_impl;
