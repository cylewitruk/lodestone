//! `~/.lodestone/config.toml` and data-directory layout.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Embedding runtime.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    CandleMetal,
    CandleCpu,
    Fastembed,
    Remote,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CandleMetal => "candle-metal",
            Self::CandleCpu => "candle-cpu",
            Self::Fastembed => "fastembed",
            Self::Remote => "remote",
        })
    }
}

impl std::str::FromStr for Backend {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "candle-metal" => Self::CandleMetal,
            "candle-cpu" => Self::CandleCpu,
            "fastembed" => Self::Fastembed,
            "remote" => Self::Remote,
            other => anyhow::bail!("unknown backend {other}"),
        })
    }
}

/// Model architecture as far as the candle backend is concerned.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Arch {
    Bert,
    JinaBert,
    NomicBert,
    ModernBert,
    /// Decoder-only transformer (Qwen2/Qwen3, from the checkpoint's `model_type`) with
    /// last-token pooling: Qwen3-Embedding, harrier-oss, jina-code-embeddings.
    Decoder,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Pooling {
    Mean,
    Cls,
    /// Hidden state of the last real token (decoder embedders).
    LastToken,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Precision {
    F32,
    F16,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelConfig {
    pub hf_repo: String,
    pub dim: usize,
    pub max_tokens: usize,
    pub arch: Arch,
    pub pooling: Pooling,
    #[serde(default)]
    pub query_prefix: String,
    #[serde(default)]
    pub document_prefix: String,
    #[serde(default)]
    pub revision: Option<String>,
    /// Name of the equivalent `fastembed::EmbeddingModel` variant, if fastembed ships it.
    #[serde(default)]
    pub fastembed: Option<String>,
    /// Compute dtype on Metal (`f16` default; some models overflow in f16 and need `f32`).
    #[serde(default)]
    pub dtype: Option<Precision>,
    /// SPDX-style license id, for `lode models`.
    #[serde(default)]
    pub license: String,
    /// Repo to take `tokenizer.json` from when the model's own repo lacks one (it must use
    /// the same vocabulary).
    #[serde(default)]
    pub tokenizer_repo: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LanguagesConfig {
    pub tree_sitter_config: Option<PathBuf>,
    pub extensions: BTreeMap<String, String>,
    pub index_untagged_as_text: Vec<String>,
}

impl Default for LanguagesConfig {
    fn default() -> Self {
        Self {
            tree_sitter_config: Some(PathBuf::from("~/.config/tree-sitter/config.json")),
            extensions: BTreeMap::new(),
            index_untagged_as_text: ["md", "txt", "adoc", "rst"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RerankConfig {
    pub enabled: bool,
    pub model: String,
    /// Hugging Face repo of the cross-encoder for the candle backends.
    pub hf_repo: String,
    /// Candidates passed to the cross-encoder. 20 suits the default embedder, which puts 92% of
    /// answers in its top 20; weaker embedders gain from 50 (see CHANGELOG, model comparison).
    pub candidates: usize,
    /// Token budget per (query, document) pair.
    pub max_tokens: usize,
    /// Runtime for the cross-encoder; defaults to the embedding backend. Non-permissively
    /// licensed rerankers (e.g. `jina-reranker-v2-base-multilingual`, CC-BY-NC-4.0) are opt-in.
    pub backend: Option<Backend>,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "bge-reranker-base".into(),
            hf_repo: "Alibaba-NLP/gte-reranker-modernbert-base".into(),
            candidates: 20,
            max_tokens: 192,
            backend: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SearchConfig {
    /// Candidates fetched from each of the vector and lexical indexes.
    pub candidates: usize,
    /// Reciprocal-rank-fusion constant.
    pub rrf_k: f32,
    /// Weight of the vector list in fusion.
    pub vector_weight: f32,
    /// Weight of each per-sentence vector list for multi-sentence queries (0 disables).
    pub sentence_weight: f32,
    /// Weight of the doc-comment vector list (needs `doc_vectors`; 0 disables). Applied only
    /// while the reranker is on: the doc list widens its candidate pool, but fused without
    /// it, documented items crowd out better code matches.
    pub doc_weight: f32,
    /// Weight of the lexical list in fusion; boosted for identifier-like queries.
    pub lexical_weight: f32,
    pub identifier_lexical_weight: f32,
    /// Exact name matches of identifier tokens in the query rank first.
    pub exact_identifier_first: bool,
    /// Score multiplier for test items when `include_tests` is set.
    pub test_weight: f32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            candidates: 200,
            rrf_k: 60.0,
            vector_weight: 1.0,
            sentence_weight: 0.0,
            doc_weight: 1.0,
            lexical_weight: 0.0,
            identifier_lexical_weight: 0.0,
            exact_identifier_first: false,
            test_weight: 0.8,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RemoteConfig {
    /// OpenAI-compatible `/v1/embeddings` endpoint.
    pub url: String,
    pub api_key_env: String,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8080/v1/embeddings".into(),
            api_key_env: "LODESTONE_REMOTE_API_KEY".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    pub model: String,
    pub backend: Backend,
    pub batch_size: usize,
    pub cache_max_gb: f64,
    /// Approximate per-item token budget for chunk text.
    pub chunk_tokens: usize,
    pub vector_precision: Precision,
    /// Chunk text is header + doc + signature only, without the body (index-time experiment).
    pub summary_chunks: bool,
    /// Emit a file-level `module` item per tagged file (index-time).
    pub module_chunks: bool,
    /// Embed each documented item's header + doc comment as a second vector (index-time).
    pub doc_vectors: bool,
    /// Lexical tokenizer (index-time; switching rebuilds the lexical index from items).
    pub lexical_tokenizer: crate::index::TokenizerKind,
    pub languages: LanguagesConfig,
    pub models: BTreeMap<String, ModelConfig>,
    pub rerank: RerankConfig,
    pub search: SearchConfig,
    pub remote: RemoteConfig,
    /// Data directory; defaults to `$LODESTONE_HOME` or `~/.lodestone`.
    pub data_dir: Option<PathBuf>,
}

/// Default embedding runtime and model for this build. On Apple GPUs the larger
/// Qwen3-Embedding-0.6B indexes fast enough (~43 chunks/s); on CPU it manages ~7 chunks/s, so
/// CPU builds default to CodeRankEmbed (~17 chunks/s). Builds without candle fall back to a
/// model fastembed ships.
pub fn default_engine() -> (Backend, &'static str) {
    if cfg!(feature = "candle") && cfg!(target_os = "macos") {
        (Backend::CandleMetal, "qwen3-0.6b")
    } else if cfg!(feature = "candle") {
        (Backend::CandleCpu, "coderank")
    } else {
        (Backend::Fastembed, "jina-code-v2")
    }
}

impl Default for Config {
    fn default() -> Self {
        let (backend, model) = default_engine();
        Self {
            port: 4600,
            model: model.into(),
            backend,
            batch_size: 64,
            cache_max_gb: 4.0,
            chunk_tokens: 200,
            vector_precision: Precision::F32,
            module_chunks: false,
            doc_vectors: false,
            summary_chunks: false,
            lexical_tokenizer: crate::index::TokenizerKind::Code,
            languages: LanguagesConfig::default(),
            models: builtin_models(),
            rerank: RerankConfig::default(),
            search: SearchConfig::default(),
            remote: RemoteConfig::default(),
            data_dir: None,
        }
    }
}

/// Task description for instruction-tuned embedders and rerankers (Qwen3 family).
pub const CODE_SEARCH_TASK: &str =
    "Given a description of what some code should do, retrieve the existing code that does it";

/// Models lodestone knows how to run without configuration.
pub fn builtin_models() -> BTreeMap<String, ModelConfig> {
    let m = |repo: &str,
             dim,
             max_tokens,
             arch,
             pooling,
             qp: &str,
             dp: &str,
             fe: Option<&str>,
             lic: &str| ModelConfig {
        hf_repo: repo.into(),
        dim,
        max_tokens,
        arch,
        pooling,
        query_prefix: qp.into(),
        document_prefix: dp.into(),
        revision: None,
        fastembed: fe.map(Into::into),
        dtype: None,
        license: lic.into(),
        tokenizer_repo: None,
    };
    BTreeMap::from([
        (
            "jina-code-v2".into(),
            m(
                "jinaai/jina-embeddings-v2-base-code",
                768,
                512,
                Arch::JinaBert,
                Pooling::Mean,
                "",
                "",
                Some("JinaEmbeddingsV2BaseCode"),
                "apache-2.0",
            ),
        ),
        (
            "coderank".into(),
            m(
                "nomic-ai/CodeRankEmbed",
                768,
                512,
                Arch::NomicBert,
                Pooling::Cls,
                "Represent this query for searching relevant code: ",
                "",
                None,
                "mit",
            ),
        ),
        (
            "gte-modernbert".into(),
            m(
                "Alibaba-NLP/gte-modernbert-base",
                768,
                512,
                Arch::ModernBert,
                Pooling::Cls,
                "",
                "",
                None,
                "apache-2.0",
            ),
        ),
        (
            "bge-small".into(),
            ModelConfig {
                dtype: Some(Precision::F32),
                ..m(
                    "BAAI/bge-small-en-v1.5",
                    384,
                    512,
                    Arch::Bert,
                    Pooling::Cls,
                    "Represent this sentence for searching relevant passages: ",
                    "",
                    Some("BGESmallENV15"),
                    "mit",
                )
            },
        ),
        (
            "granite-r2".into(),
            m(
                "ibm-granite/granite-embedding-english-r2",
                768,
                512,
                Arch::ModernBert,
                Pooling::Cls,
                "",
                "",
                None,
                "apache-2.0",
            ),
        ),
        (
            "granite-small-r2".into(),
            m(
                "ibm-granite/granite-embedding-small-english-r2",
                384,
                512,
                Arch::ModernBert,
                Pooling::Cls,
                "",
                "",
                None,
                "apache-2.0",
            ),
        ),
        (
            "minilm-l6".into(),
            ModelConfig {
                dtype: Some(Precision::F32),
                ..m(
                    "sentence-transformers/all-MiniLM-L6-v2",
                    384,
                    256,
                    Arch::Bert,
                    Pooling::Mean,
                    "",
                    "",
                    Some("AllMiniLML6V2"),
                    "apache-2.0",
                )
            },
        ),
        (
            "qwen3-0.6b".into(),
            m(
                "Qwen/Qwen3-Embedding-0.6B",
                1024,
                512,
                Arch::Decoder,
                Pooling::LastToken,
                &format!("Instruct: {CODE_SEARCH_TASK}\nQuery:"),
                "",
                None,
                "apache-2.0",
            ),
        ),
        (
            "harrier-0.6b".into(),
            m(
                "microsoft/harrier-oss-v1-0.6b",
                1024,
                512,
                Arch::Decoder,
                Pooling::LastToken,
                &format!("Instruct: {CODE_SEARCH_TASK}\nQuery: "),
                "",
                None,
                "mit",
            ),
        ),
        (
            "jina-code-0.5b".into(),
            ModelConfig {
                // The repo ships vocab.json/merges.txt only; Qwen2.5-Coder's tokenizer has the
                // identical vocabulary and merges.
                tokenizer_repo: Some("Qwen/Qwen2.5-Coder-0.5B".into()),
                ..m(
                    "jinaai/jina-code-embeddings-0.5b",
                    896,
                    512,
                    Arch::Decoder,
                    Pooling::LastToken,
                    "Find the most relevant code snippet given the following query:\n",
                    "Candidate code snippet:\n",
                    None,
                    "cc-by-nc-4.0",
                )
            },
        ),
        (
            "nomic-text-v1.5".into(),
            m(
                "nomic-ai/nomic-embed-text-v1.5",
                768,
                512,
                Arch::NomicBert,
                Pooling::Mean,
                "search_query: ",
                "search_document: ",
                Some("NomicEmbedTextV15"),
                "apache-2.0",
            ),
        ),
    ])
}

impl Config {
    /// Loads the config file if it exists (defaults otherwise); user `[models]` entries are
    /// merged over the built-in table.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_data_dir().join("config.toml"));
        let mut cfg = if path.exists() {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let mut parsed: Config =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            let mut models = builtin_models();
            models.append(&mut parsed.models);
            parsed.models = models;
            parsed
        } else {
            Config::default()
        };
        if cfg.data_dir.is_none() {
            cfg.data_dir = Some(default_data_dir());
        }
        Ok(cfg)
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(default_data_dir)
    }

    pub fn model_config(&self, alias: &str) -> anyhow::Result<&ModelConfig> {
        self.models
            .get(alias)
            .with_context(|| format!("unknown model {alias}; see `lode models`"))
    }
}

pub fn default_data_dir() -> PathBuf {
    std::env::var_os("LODESTONE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".lodestone"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_runs_on_default_backend() {
        let c = Config::default();
        let m = c.model_config(&c.model).unwrap();
        if c.backend == Backend::Fastembed {
            assert!(m.fastembed.is_some(), "{} has no fastembed build", c.model);
        }
        assert!(
            !m.license.contains("nc"),
            "default model must be permissively licensed"
        );
    }

    #[test]
    fn parses_plan_example_config() {
        let text = r#"
port = 4600
model = "jina-code-v2"
backend = "candle-metal"
batch_size = 64
cache_max_gb = 4

[languages]
tree_sitter_config = "~/.config/tree-sitter/config.json"
extensions = { "cljc" = "clojure" }
index_untagged_as_text = ["md", "txt", "adoc"]

[models.jina-code-v2]
hf_repo = "jinaai/jina-embeddings-v2-base-code"
dim = 768
max_tokens = 512
arch = "jina-bert"
pooling = "mean"

[rerank]
enabled = false
model = "bge-reranker-v2-m3"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.backend, Backend::CandleMetal);
        assert_eq!(cfg.languages.extensions["cljc"], "clojure");
        assert_eq!(cfg.models["jina-code-v2"].dim, 768);
        let back: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
    }
}
