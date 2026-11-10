//! Registry of worktrees and the engine shared by the daemon and the in-process CLI. This is the
//! only place paths are canonicalized (invariant 2).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::api::types::{
    ExtensionCount, RepoInfo, SearchRequest, SearchResponse, SearchResult, StatusReport,
};
use crate::cache::EmbeddingCache;
use crate::chunk::{Chunker, LanguageRegistry};
use crate::config::Config;
use crate::embed::{self, Embedder, InputKind};
use crate::index::{IndexContext, IndexOptions, IndexReport, WorktreeIndex};
use crate::search::rerank::Reranker;
use crate::search::{self, Filters, SearchParams, format};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct RegistryFile {
    repos: Vec<RepoEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct RepoEntry {
    root: PathBuf,
    name: String,
}

fn read_registry(path: &Path) -> RegistryFile {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Registered worktree roots, read without starting an engine.
pub fn registered_roots(config: &Config) -> Vec<PathBuf> {
    read_registry(&config.data_dir().join("registry.json"))
        .repos
        .into_iter()
        .map(|r| r.root)
        .collect()
}

/// The registered worktree containing `dir` (the innermost, for nested worktrees).
pub fn root_of(roots: &[PathBuf], dir: &Path) -> Option<PathBuf> {
    let dir = dir.canonicalize().ok()?;
    roots
        .iter()
        .filter(|r| dir.starts_with(r))
        .max_by_key(|r| r.as_os_str().len())
        .cloned()
}

pub struct Engine {
    pub config: Config,
    pub languages: Arc<LanguageRegistry>,
    pub chunker: Chunker,
    pub cache: Arc<EmbeddingCache>,
    model_id: String,
    dim: usize,
    embedder: OnceLock<Arc<dyn Embedder>>,
    embedder_override: Option<Arc<dyn Embedder>>,
    reranker: OnceLock<Option<Arc<dyn Reranker>>>,
    worktrees: RwLock<BTreeMap<PathBuf, Arc<WorktreeIndex>>>,
    /// Watcher state per worktree, reported by `status` (set by the daemon).
    pub watcher_state: RwLock<BTreeMap<PathBuf, String>>,
}

/// Canonicalizes a user-supplied path. Called only at the registry boundary.
pub fn canonicalize(path: &Path) -> anyhow::Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("no such directory: {}", path.display()))
}

impl Engine {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        Self::build(config, None)
    }

    /// `new`, except `LODESTONE_TEST_EMBEDDER=hash` substitutes a deterministic embedder so the
    /// binary can be exercised end to end without model downloads.
    pub fn from_env(config: Config) -> anyhow::Result<Self> {
        match std::env::var("LODESTONE_TEST_EMBEDDER").as_deref() {
            Ok("hash") => {
                let mut config = config;
                config.rerank.enabled = false;
                let delay_ms = std::env::var("LODESTONE_TEST_EMBED_DELAY_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                Self::with_embedder(
                    config,
                    Arc::new(embed::HashEmbedder {
                        dim: 64,
                        id: "test/hash".into(),
                        delay_ms,
                    }),
                )
            }
            _ => Self::new(config),
        }
    }

    /// Engine with a fixed embedder (tests, benchmarks with an already-loaded model).
    pub fn with_embedder(config: Config, embedder: Arc<dyn Embedder>) -> anyhow::Result<Self> {
        Self::build(config, Some(embedder))
    }

    fn build(config: Config, embedder_override: Option<Arc<dyn Embedder>>) -> anyhow::Result<Self> {
        let data = config.data_dir();
        fs::create_dir_all(&data)?;
        let languages = Arc::new(LanguageRegistry::load(&config.languages, &data));
        let chunker = Chunker::new(languages.clone(), config.chunk_tokens)
            .with_module_chunks(config.module_chunks)
            .with_summary_only(config.summary_chunks);
        let (model_id, dim) = match &embedder_override {
            Some(e) => (e.model_id().to_string(), e.dim()),
            None => {
                let m = config.model_config(&config.model)?;
                (embed::model_id(m), m.dim)
            }
        };
        Ok(Self {
            cache: Arc::new(EmbeddingCache::open(&data.join("cache.sqlite"))?),
            languages,
            chunker,
            model_id,
            dim,
            embedder: OnceLock::new(),
            embedder_override,
            reranker: OnceLock::new(),
            worktrees: RwLock::new(BTreeMap::new()),
            watcher_state: RwLock::new(BTreeMap::new()),
            config,
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// The resident embedder, loaded on first use.
    pub fn embedder(&self) -> anyhow::Result<Arc<dyn Embedder>> {
        if let Some(e) = &self.embedder_override {
            return Ok(e.clone());
        }
        if let Some(e) = self.embedder.get() {
            return Ok(e.clone());
        }
        let e = embed::load(&self.config, &self.config.model, self.config.backend)?;
        Ok(self.embedder.get_or_init(|| e).clone())
    }

    pub fn reranker(&self) -> Option<Arc<dyn Reranker>> {
        self.reranker
            .get_or_init(|| {
                if !self.config.rerank.enabled {
                    return None;
                }
                let data = self.config.data_dir().join("models");
                let backend = self.config.rerank.backend.unwrap_or(self.config.backend);
                let loaded: anyhow::Result<Arc<dyn Reranker>> = match backend {
                    #[cfg(feature = "candle")]
                    crate::config::Backend::CandleMetal | crate::config::Backend::CandleCpu => {
                        search::rerank::candle_impl::CandleReranker::load(
                            &self.config.rerank.hf_repo,
                            backend,
                            &data,
                            self.config.rerank.max_tokens,
                        )
                        .map(|r| Arc::new(r) as Arc<dyn Reranker>)
                    }
                    #[cfg(feature = "fastembed")]
                    _ => search::rerank::fastembed_impl::FastembedReranker::load(
                        &self.config.rerank.model,
                        &data,
                    )
                    .map(|r| Arc::new(r) as Arc<dyn Reranker>),
                    #[allow(unreachable_patterns)]
                    _ => Err(anyhow::anyhow!("no reranker backend compiled in")),
                };
                match loaded {
                    Ok(r) => Some(r),
                    Err(e) => {
                        tracing::warn!("reranker unavailable: {e:#}");
                        None
                    }
                }
            })
            .clone()
    }

    pub fn index_context(&self) -> anyhow::Result<IndexContext> {
        Ok(IndexContext {
            chunker: self.chunker.clone(),
            embedder: self.embedder()?,
            cache: self.cache.clone(),
            config: self.config.clone(),
        })
    }

    fn registry_path(&self) -> PathBuf {
        self.config.data_dir().join("registry.json")
    }

    fn registry_file(&self) -> RegistryFile {
        read_registry(&self.registry_path())
    }

    pub fn repos(&self) -> Vec<RepoInfo> {
        self.registry_file()
            .repos
            .into_iter()
            .map(|r| RepoInfo {
                worktree_id: crate::index::worktree_id(&r.root),
                root: r.root.to_string_lossy().to_string(),
                name: r.name,
            })
            .collect()
    }

    /// Adds `path` to the registry (idempotent) and returns its index handle.
    pub fn register(&self, path: &Path) -> anyhow::Result<Arc<WorktreeIndex>> {
        let root = canonicalize(path)?;
        anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
        let mut reg = self.registry_file();
        if !reg.repos.iter().any(|r| r.root == root) {
            let base = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut name = base.clone();
            let mut n = 2;
            while reg.repos.iter().any(|r| r.name == name) {
                name = format!("{base}-{n}");
                n += 1;
            }
            reg.repos.push(RepoEntry {
                root: root.clone(),
                name,
            });
            let tmp = self.registry_path().with_extension("json.tmp");
            fs::write(&tmp, serde_json::to_vec_pretty(&reg)?)?;
            fs::rename(tmp, self.registry_path())?;
        }
        self.open_canonical(&root)
    }

    /// Index handle for an already-canonical root, opened once per process.
    pub fn open_canonical(&self, root: &Path) -> anyhow::Result<Arc<WorktreeIndex>> {
        if let Some(w) = self
            .worktrees
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .get(root)
        {
            return Ok(w.clone());
        }
        let mut map = self
            .worktrees
            .write()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        if let Some(w) = map.get(root) {
            return Ok(w.clone());
        }
        let w = Arc::new(WorktreeIndex::open(
            root,
            &self.config,
            &self.model_id,
            self.dim,
        )?);
        map.insert(root.to_path_buf(), w.clone());
        Ok(w)
    }

    /// Resolves the `repo` argument: a registered name, a path (canonicalized here), or — when
    /// absent — the only registered repo.
    pub fn resolve(&self, repo: Option<&str>) -> anyhow::Result<Arc<WorktreeIndex>> {
        let reg = self.registry_file();
        match repo {
            None => match reg.repos.as_slice() {
                [only] => self.open_canonical(&only.root),
                [] => bail!("no repositories registered; run `lode register <path>`"),
                many => bail!(
                    "{} repositories registered; pass repo (one of: {})",
                    many.len(),
                    many.iter()
                        .map(|r| r.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
            Some(r) => {
                if let Some(e) = reg.repos.iter().find(|e| e.name == r) {
                    return self.open_canonical(&e.root);
                }
                let root = canonicalize(Path::new(r))?;
                // A path inside a registered worktree resolves to that worktree.
                if let Some(e) = reg
                    .repos
                    .iter()
                    .filter(|e| root.starts_with(&e.root))
                    .max_by_key(|e| e.root.as_os_str().len())
                {
                    return self.open_canonical(&e.root);
                }
                self.open_canonical(&root)
            }
        }
    }

    pub fn repo_name(&self, root: &Path) -> String {
        self.registry_file()
            .repos
            .into_iter()
            .find(|r| r.root == root)
            .map(|r| r.name)
            .unwrap_or_else(|| {
                root.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            })
    }

    pub fn index_blocking(
        &self,
        w: &WorktreeIndex,
        opts: IndexOptions,
    ) -> anyhow::Result<IndexReport> {
        let ctx = self.index_context()?;
        w.index_blocking(&ctx, opts)
    }

    /// Runs a search. Never indexes (invariant 1).
    pub fn search(&self, req: &SearchRequest) -> anyhow::Result<SearchResponse> {
        let started = Instant::now();
        let w = self.resolve(req.repo.as_deref())?;
        let snap = w.snapshot();
        let mut notes = Vec::new();
        let sentences = if self.config.search.sentence_weight > 0.0 {
            search::sentences(&req.query, 4)
        } else {
            Vec::new()
        };
        let mut sentence_vecs: Vec<Vec<f32>> = Vec::new();
        let query_vec = if snap.items.is_empty() {
            None
        } else if snap.model_id != self.model_id {
            notes.push(
                "index model differs from configured model; lexical results only".to_string(),
            );
            None
        } else {
            let mut texts = vec![req.query.as_str()];
            texts.extend(sentences.iter().map(String::as_str));
            match self
                .embedder()
                .and_then(|e| e.embed(&texts, InputKind::Query))
            {
                Ok(mut v) => {
                    sentence_vecs = v.split_off(1);
                    v.pop()
                }
                Err(e) => {
                    notes.push(format!(
                        "embedding unavailable ({e:#}); lexical results only"
                    ));
                    None
                }
            }
        };
        let filters = Filters {
            from_unit: req.from_unit.clone(),
            path_prefix: req.path_prefix.clone(),
            language: req.language.clone(),
            kinds: req.kinds.clone(),
            include_tests: req.include_tests,
        };
        let top_k = req.top_k.clamp(1, 50);
        let reranker = self.reranker();
        let mut search_config = self.config.search.clone();
        if reranker.is_none() {
            search_config.doc_weight = 0.0;
        }
        let mut outcome = search::search(
            &snap,
            w.lexical(),
            &SearchParams {
                query: &req.query,
                query_vector: query_vec.as_deref(),
                sentence_vectors: &sentence_vecs,
                filters: &filters,
                top_k,
                config: &search_config,
            },
        )?;
        if let Some(rr) = reranker {
            let n = self.config.rerank.candidates.min(outcome.hits.len());
            if n > 1 {
                let docs: Vec<String> = outcome.hits[..n]
                    .iter()
                    .map(|h| h.row.item.text.clone())
                    .collect();
                let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
                match rr.score(&req.query, &refs) {
                    Ok(scores) => {
                        let mut head: Vec<_> = outcome.hits.drain(..n).zip(scores).collect();
                        head.sort_by(|a, b| b.1.total_cmp(&a.1));
                        let exact = |h: &search::Hit| h.score >= 10.0;
                        // Exact identifier matches stay on top.
                        head.sort_by_key(|(h, _)| !exact(h));
                        let mut rest = std::mem::take(&mut outcome.hits);
                        outcome.hits = head.into_iter().map(|(h, _)| h).collect();
                        outcome.hits.append(&mut rest);
                    }
                    Err(e) => notes.push(format!("rerank failed: {e:#}")),
                }
            }
        }
        notes.extend(outcome.notes);
        let results = outcome
            .hits
            .into_iter()
            .take(top_k)
            .map(|h| {
                let it = &h.row.item;
                SearchResult {
                    path: it.rel_path.clone(),
                    line: it.line_start,
                    line_end: it.line_end,
                    name_line: it.name_line,
                    name_column: it.name_column,
                    kind: it.kind.clone(),
                    name: it.name.clone(),
                    qualified_name: it.qualified_name.clone(),
                    signature: it.signature.clone(),
                    doc: it.doc_first_line().to_string(),
                    unit: h.row.unit.clone(),
                    language: it.language.clone(),
                    is_test: it.is_test,
                    score: h.score,
                    similarity: h.similarity,
                    text: format::line(it),
                    body: req.with_body.map(|n| format::body(&w.root, it, n)),
                }
            })
            .collect();
        let mut stale_reasons = snap.stale.clone();
        if w.is_writing() {
            stale_reasons.push("indexing in progress".into());
        }
        Ok(SearchResponse {
            repo: w.root.to_string_lossy().to_string(),
            stale: !stale_reasons.is_empty(),
            stale_reasons,
            notes,
            results,
            took_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    }

    pub fn status(&self, repo: Option<&str>) -> anyhow::Result<StatusReport> {
        let w = self.resolve(repo)?;
        let snap = w.snapshot();
        let languages = self.languages.clone();
        let pending = w.pending_files(&|p: &Path| languages.accepts(p));
        let mut stale_reasons = snap.stale.clone();
        if pending > 0 {
            stale_reasons.push(format!("{pending} files changed since last index"));
        }
        let progress = w.progress();
        if progress.running {
            stale_reasons.push("indexing in progress".into());
        }
        let watcher = self
            .watcher_state
            .read()
            .ok()
            .and_then(|m| m.get(&w.root).cloned())
            .unwrap_or_else(|| "none (in-process)".into());
        Ok(StatusReport {
            repo: w.root.to_string_lossy().to_string(),
            name: self.repo_name(&w.root),
            worktree_id: w.id.clone(),
            model: self.model_id.clone(),
            backend: self.config.backend.to_string(),
            index_model: snap.model_id.clone(),
            files_indexed: snap.files_indexed,
            items: snap.items.len(),
            vectors: snap.vectors.len(),
            doc_vectors: snap.doc_vectors.len(),
            pending_changes: pending,
            last_update: snap.updated_at,
            stale: !stale_reasons.is_empty(),
            stale_reasons,
            indexing: progress,
            last_report: w.last_report(),
            graph_providers: snap.graph.providers.clone(),
            units: snap.graph.units.len(),
            layer_filter: snap.graph.has_edges(),
            watcher,
            extensions: w
                .extension_counts()
                .into_iter()
                .map(|(k, (t, x, s))| {
                    (
                        k,
                        ExtensionCount {
                            tagged: t,
                            text: x,
                            skipped_files: s,
                        },
                    )
                })
                .collect(),
        })
    }

    pub fn worktrees(&self) -> Vec<Arc<WorktreeIndex>> {
        self.worktrees
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
}
