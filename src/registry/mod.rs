//! Registry of worktrees and the engine shared by the daemon and the in-process CLI. This is the
//! only place paths are canonicalized (invariant 2).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::Instant;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::api::types::{
    ExtensionCount, RepoFailure, RepoInfo, SearchRequest, SearchResponse, SearchResult,
    StatusOverview, StatusReport,
};
use crate::cache::EmbeddingCache;
use crate::checkout::Checkout;
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
    /// Registered automatically, as a linked git worktree of a registered repository; dropped,
    /// index and all, once its directory is gone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    auto: bool,
    /// Being unregistered: no longer served, kept until its index is deleted, so that a removal
    /// interrupted part-way is finished by the next (see [`Engine::tidy_worktrees`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    removing: bool,
}

impl RegistryFile {
    /// The entries registered, without those being removed.
    fn live(&self) -> impl Iterator<Item = &RepoEntry> {
        self.repos.iter().filter(|r| !r.removing)
    }
}

impl RepoEntry {
    fn info(self) -> RepoInfo {
        RepoInfo {
            worktree_id: crate::index::worktree_id(&self.root),
            root: self.root.to_string_lossy().to_string(),
            name: self.name,
        }
    }
}

/// The registry at `path`; empty when the file does not exist yet. Any other failure is an
/// error, so that nothing writes over a registry it could not read.
fn read_registry(path: &Path) -> anyhow::Result<RegistryFile> {
    let bytes = match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RegistryFile::default()),
        r => r.with_context(|| format!("reading {}", path.display()))?,
    };
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "{} is not a valid registry; fix or remove it",
            path.display()
        )
    })
}

/// [`read_registry`] for listing: only the worktrees registered (see [`RegistryFile::live`]); an
/// unreadable registry lists nothing, and is logged.
fn list_registry(path: &Path) -> RegistryFile {
    let mut reg = read_registry(path).unwrap_or_else(|e| {
        tracing::warn!("{e:#}");
        RegistryFile::default()
    });
    reg.repos.retain(|r| !r.removing);
    reg
}

/// Registered worktree roots, read without starting an engine.
pub fn registered_roots(config: &Config) -> Vec<PathBuf> {
    list_registry(&config.data_dir().join("registry.json"))
        .repos
        .into_iter()
        .map(|r| r.root)
        .collect()
}

/// Registered worktrees as (name, root), read without starting an engine.
pub fn registered(config: &Config) -> Vec<(String, PathBuf)> {
    list_registry(&config.data_dir().join("registry.json"))
        .repos
        .into_iter()
        .map(|r| (r.name, r.root))
        .collect()
}

/// The worktrees among `repos` (name, root) inside `dir`, a folder that holds several
/// repositories.
pub fn inside(repos: Vec<(String, PathBuf)>, dir: &Path) -> Vec<(String, PathBuf)> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    repos
        .into_iter()
        .filter(|(_, root)| root.starts_with(&dir))
        .collect()
}

/// `repos` (name, root) for a message: "`name` (root), …".
pub fn listing(repos: &[(String, PathBuf)]) -> String {
    repos
        .iter()
        .map(|(name, root)| format!("`{name}` ({})", root.display()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The top-level directory of the git checkout `dir` is in.
pub fn checkout_top(dir: &Path) -> Option<PathBuf> {
    Checkout::of(dir).map(|c| c.top)
}

/// The shared git directory of the repository `dir` is in, and whether `dir` is in a linked
/// worktree (`git worktree add`) rather than the main checkout.
pub fn git_repository(dir: &Path) -> Option<(PathBuf, bool)> {
    Checkout::of(dir).map(|c| {
        let linked = c.is_linked();
        (c.common, linked)
    })
}

/// Which index serves a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// This registered worktree's.
    Registered(PathBuf),
    /// A linked git worktree (top-level directory) of a registered repository, not registered
    /// yet; it gets its own index.
    NewWorktree(PathBuf),
    /// A git checkout (top-level directory) that no registered worktree covers.
    Unregistered(PathBuf),
    /// Neither in a git checkout nor in a registered directory.
    Outside,
}

/// Decides which index serves `dir`, never one built from a different checkout: a linked git
/// worktree (`git worktree add`) is served only by its own index, even inside a registered
/// checkout, and so is a separate repository nested in one. Git submodules belong to the
/// checkout that contains them.
pub fn place(roots: &[PathBuf], dir: &Path) -> Placement {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let checkout = Checkout::of(&dir);
    if let Some(c) = checkout.as_ref().filter(|c| c.is_linked()) {
        return if roots.contains(&c.top) {
            Placement::Registered(c.top.clone())
        } else if roots
            .iter()
            .any(|r| Checkout::of(r).is_some_and(|rc| rc.common == c.common))
        {
            Placement::NewWorktree(c.top.clone())
        } else {
            Placement::Unregistered(c.top.clone())
        };
    }
    match (root_of(roots, &dir), checkout) {
        (Some(root), Some(c))
            if c.top != root
                && c.top.starts_with(&root)
                && c.is_separate_from(Checkout::of(&root).as_ref()) =>
        {
            Placement::Unregistered(c.top)
        }
        (Some(root), _) => Placement::Registered(root),
        (None, Some(c)) => Placement::Unregistered(c.top),
        (None, None) => Placement::Outside,
    }
}

/// A registry name for `root`: its directory name, or for a linked git worktree of a registered
/// repository `<repository>:<worktree>`, after the repository's main checkout.
fn base_name(reg: &RegistryFile, root: &Path) -> String {
    let dir = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(c) = Checkout::of(root).filter(Checkout::is_linked) else {
        return dir;
    };
    reg.repos
        .iter()
        .find(|r| Checkout::of(&r.root).is_some_and(|m| m.common == c.common && !m.is_linked()))
        .map_or(dir.clone(), |main| format!("{}:{dir}", main.name))
}

/// `base`, or `base-2`, `base-3`, … if another entry than `root` has it.
fn unique_name(reg: &RegistryFile, base: &str, root: &Path) -> String {
    let taken = |name: &str| reg.repos.iter().any(|r| r.name == name && r.root != root);
    let mut name = base.to_string();
    let mut n = 2;
    while taken(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

/// The entry `repo` names in `reg`, including one being removed: a registered name; else the
/// path `repo` (relative to `cwd`; by default `cwd` itself), when it exists, of the registered
/// worktree it is the root of or is in (see [`place`]); else the registered root it names once
/// resolved (see [`resolve_missing`]). `cwd` is needed only for a relative path or the default.
fn find(
    reg: &RegistryFile,
    repo: Option<&str>,
    cwd: Option<&Path>,
) -> anyhow::Result<Option<usize>> {
    if let Some(i) = reg.repos.iter().position(|r| Some(r.name.as_str()) == repo) {
        return Ok(Some(i));
    }
    let path = match (repo, cwd) {
        (Some(r), _) if Path::new(r).is_absolute() => PathBuf::from(r),
        (Some(r), Some(cwd)) => cwd.join(r),
        (None, Some(cwd)) => cwd.to_path_buf(),
        (_, None) => {
            bail!("the current directory is gone: name the worktree, or give its absolute path")
        }
    };
    let path = std::path::absolute(path)?;
    let at = |root: &Path| reg.repos.iter().position(|r| r.root == root);
    if !path.exists() {
        return Ok(resolve_missing(&path).and_then(|p| at(&p)));
    }
    let path = canonicalize(&path)?;
    if let Some(i) = at(&path) {
        return Ok(Some(i));
    }
    let roots: Vec<PathBuf> = reg.repos.iter().map(|r| r.root.clone()).collect();
    Ok(match place(&roots, &path) {
        Placement::Registered(root) => at(&root),
        _ => None,
    })
}

/// An absolute path that does not exist, canonical up to its nearest existing ancestor and
/// resolved lexically below it (where there are no symlinks to follow).
fn resolve_missing(path: &Path) -> Option<PathBuf> {
    let base = path.ancestors().find(|a| a.exists())?;
    let mut out = base.canonicalize().ok()?;
    for c in path.strip_prefix(base).ok()?.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    Some(out)
}

/// The registered worktree containing `dir` (the innermost, for nested worktrees).
pub fn root_of(roots: &[PathBuf], dir: &Path) -> Option<PathBuf> {
    // A directory that no longer exists (e.g. a removed worktree) is matched as given.
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    roots
        .iter()
        .filter(|r| dir.starts_with(r))
        .max_by_key(|r| r.as_os_str().len())
        .cloned()
}

/// Held while `registry.json` is read, updated and written (see [`Engine::lock_registry`]).
struct RegistryLock<'a> {
    _thread: MutexGuard<'a, ()>,
    _process: fs::File,
}

/// The registry lock held shared (see [`Engine::share_registry`]).
struct RegistryShare {
    _file: fs::File,
}

/// A loaded reranker and its model, or why it failed to load.
type LoadedReranker = Result<(Arc<dyn Reranker>, String), String>;

pub struct Engine {
    pub config: Config,
    pub languages: Arc<LanguageRegistry>,
    pub chunker: Chunker,
    pub cache: Arc<EmbeddingCache>,
    model_id: String,
    dim: usize,
    embedder: OnceLock<Arc<dyn Embedder>>,
    embedder_override: Option<Arc<dyn Embedder>>,
    /// The reranker and its model, or why it failed to load; unset until first use or while
    /// disabled.
    reranker: OnceLock<LoadedReranker>,
    worktrees: RwLock<BTreeMap<PathBuf, Arc<WorktreeIndex>>>,
    /// Watcher state per worktree, reported by `status` (set by the daemon).
    pub watcher_state: RwLock<BTreeMap<PathBuf, String>>,
    /// Serializes this process's read-modify-write updates of `registry.json` (see
    /// [`Engine::lock_registry`]).
    registry_writes: Mutex<()>,
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
    /// binary can be exercised end to end without model downloads; it fails documents while the
    /// file `LODESTONE_TEST_EMBED_FAIL_FLAG` names exists.
    pub fn from_env(config: Config) -> anyhow::Result<Self> {
        match std::env::var("LODESTONE_TEST_EMBEDDER").as_deref() {
            Ok("hash") => {
                let mut config = config;
                config.rerank.enabled = false;
                let delay_ms = std::env::var("LODESTONE_TEST_EMBED_DELAY_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let hash: Arc<dyn Embedder> = Arc::new(embed::HashEmbedder {
                    dim: 64,
                    id: "test/hash".into(),
                    delay_ms,
                });
                let embedder = match std::env::var_os("LODESTONE_TEST_EMBED_FAIL_FLAG") {
                    Some(flag) => Arc::new(crate::testutil::FailWhile {
                        inner: hash,
                        flag: flag.into(),
                    }),
                    None => hash,
                };
                Self::with_embedder(config, embedder)
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
            registry_writes: Mutex::new(()),
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

    /// The resident reranker, loaded on first use; `None` when disabled or it failed to load (see
    /// [`Self::reranker_error`]).
    pub fn reranker(&self) -> Option<Arc<dyn Reranker>> {
        Some(self.load_reranker()?.as_ref().ok()?.0.clone())
    }

    /// The model the resident reranker runs, if one is loaded.
    pub fn reranker_model(&self) -> Option<&str> {
        Some(&self.load_reranker()?.as_ref().ok()?.1)
    }

    /// Why the enabled reranker is unavailable.
    pub fn reranker_error(&self) -> Option<String> {
        self.load_reranker()?.as_ref().err().cloned()
    }

    fn load_reranker(&self) -> Option<&LoadedReranker> {
        if !self.config.rerank.enabled {
            return None;
        }
        Some(self.reranker.get_or_init(|| {
            let data = self.config.data_dir().join("models");
            let rerank = &self.config.rerank;
            let backend = rerank.backend.unwrap_or(self.config.backend);
            let loaded: anyhow::Result<(Arc<dyn Reranker>, String)> = match backend {
                #[cfg(feature = "candle")]
                crate::config::Backend::CandleMetal | crate::config::Backend::CandleCpu => {
                    search::rerank::candle_impl::CandleReranker::load(
                        &rerank.hf_repo,
                        backend,
                        &data,
                        rerank.max_tokens,
                    )
                    .map(|r| (Arc::new(r) as Arc<dyn Reranker>, rerank.hf_repo.clone()))
                }
                #[cfg(feature = "fastembed")]
                _ => search::rerank::fastembed_impl::FastembedReranker::load(&rerank.model, &data)
                    .map(|r| (Arc::new(r) as Arc<dyn Reranker>, rerank.model.clone())),
                #[allow(unreachable_patterns)]
                _ => Err(anyhow::anyhow!("no reranker backend compiled in")),
            };
            loaded.map_err(|e| {
                tracing::warn!("reranker unavailable: {e:#}");
                format!("{e:#}")
            })
        }))
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

    fn registry_file(&self) -> anyhow::Result<RegistryFile> {
        read_registry(&self.registry_path())
    }

    pub fn repos(&self) -> Vec<RepoInfo> {
        list_registry(&self.registry_path())
            .repos
            .into_iter()
            .map(RepoEntry::info)
            .collect()
    }

    /// Adds `path` to the registry (idempotent) and returns its index handle.
    pub fn register(&self, path: &Path) -> anyhow::Result<Arc<WorktreeIndex>> {
        self.add(path, false)
    }

    /// The index serving `dir` (see [`place`]). A linked git worktree of a registered repository
    /// is registered on first use; a directory no registered worktree covers is an error.
    pub fn worktree_for(&self, dir: &Path) -> anyhow::Result<Arc<WorktreeIndex>> {
        let roots: Vec<PathBuf> = self
            .registry_file()?
            .live()
            .map(|r| r.root.clone())
            .collect();
        match place(&roots, dir) {
            Placement::Registered(root) => self.open_canonical(&root),
            Placement::NewWorktree(top) => self.add(&top, true),
            Placement::Unregistered(top) => bail!(
                "{} is not indexed by lodestone; run `lode register {}`",
                top.display(),
                top.display()
            ),
            Placement::Outside => bail!(
                "{} is not in a registered worktree; run `lode register <path>`",
                dir.display()
            ),
        }
    }

    /// The worktree `lode index <path>` means: the registered worktree containing `path` (a
    /// linked worktree of a registered repository is registered first), else `path` itself,
    /// registered.
    pub fn index_target(&self, path: &Path) -> anyhow::Result<Arc<WorktreeIndex>> {
        self.worktree_for(path).or_else(|_| self.register(path))
    }

    /// Unregisters automatically registered worktrees whose directory is gone and deletes their
    /// indexes, finishes removals interrupted part-way, and names the others after their
    /// repository (`<repository>:<worktree>`). Returns the roots unregistered. Must not be called
    /// from async context.
    pub fn tidy_worktrees(&self) -> anyhow::Result<Vec<PathBuf>> {
        let _write = self.lock_registry()?;
        let mut reg = self.registry_file()?;
        // An entry already marked was left by a removal that stopped: none is under way while
        // this holds the registry lock.
        for r in &mut reg.repos {
            r.removing |= r.auto && !r.root.exists();
        }
        let mut renamed = false;
        for i in 0..reg.repos.len() {
            let r = &reg.repos[i];
            if r.removing {
                continue;
            }
            let base = base_name(&reg, &r.root);
            let suffixed = r
                .name
                .strip_prefix(&format!("{base}-"))
                .is_some_and(|n| n.parse::<u32>().is_ok());
            if r.auto && r.name != base && !suffixed {
                reg.repos[i].name = unique_name(&reg, &base, &reg.repos[i].root.clone());
                renamed = true;
            }
        }
        if !renamed && !reg.repos.iter().any(|r| r.removing) {
            return Ok(Vec::new());
        }
        let mut removed = Vec::new();
        for (root, deleted) in self.unregister_marked(reg)? {
            match deleted {
                Ok(()) => removed.push(root),
                Err(e) => tracing::warn!("{e:#}"),
            }
        }
        Ok(removed)
    }

    /// Unregisters `repo` (a registered name, or a path relative to `cwd` in a registered
    /// worktree or naming the root of one since deleted; by default `cwd`) and deletes its index.
    /// `cwd` is needed only for a relative path or the default. A failure to delete it leaves it
    /// registered; one that stops part-way is finished by the next attempt. Must not be called
    /// from async context.
    pub fn unregister(&self, repo: Option<&str>, cwd: Option<&Path>) -> anyhow::Result<RepoInfo> {
        let _write = self.lock_registry()?;
        let mut reg = self.registry_file()?;
        let Some(i) = find(&reg, repo, cwd)? else {
            bail!(
                "{} is not a registered worktree",
                repo.map_or_else(
                    || cwd.unwrap_or(Path::new(".")).display().to_string(),
                    str::to_string
                )
            );
        };
        reg.repos[i].removing = true;
        let entry = reg.repos[i].clone();
        for (root, deleted) in self.unregister_marked(reg)? {
            if root == entry.root {
                deleted?;
            }
        }
        Ok(entry.info())
    }

    /// Writes `reg`, then deletes the indexes of its entries marked `removing` and drops them. An
    /// entry whose index could not be deleted is registered again, so that removing it can be
    /// retried. Returns each marked root with its outcome. The caller holds
    /// [`Self::lock_registry`], so nothing loads their indexes meanwhile (see [`Self::load`]).
    /// Must not be called from async context.
    fn unregister_marked(
        &self,
        mut reg: RegistryFile,
    ) -> anyhow::Result<Vec<(PathBuf, anyhow::Result<()>)>> {
        self.write_registry(&reg)?;
        let roots: Vec<PathBuf> = reg
            .repos
            .iter()
            .filter(|r| r.removing)
            .map(|r| r.root.clone())
            .collect();
        let closed: Vec<Arc<WorktreeIndex>> = {
            let mut open = self
                .worktrees
                .write()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            roots.iter().filter_map(|r| open.remove(r)).collect()
        };
        let outcomes: Vec<(PathBuf, anyhow::Result<()>)> = roots
            .into_iter()
            .map(|root| {
                let deleted = self.delete_index(&root, &closed);
                (root, deleted)
            })
            .collect();
        let errors: Vec<String> = outcomes
            .iter()
            .filter_map(|(_, deleted)| Some(format!("{:#}", deleted.as_ref().err()?)))
            .collect();
        let context = || match errors.as_slice() {
            [] => "finishing the removal".to_string(),
            e => format!(
                "registering again what could not be deleted ({})",
                e.join("; ")
            ),
        };
        reg.repos.retain_mut(|r| {
            let failed = outcomes
                .iter()
                .any(|(root, deleted)| *root == r.root && deleted.is_err());
            let keep = !r.removing || failed;
            r.removing = false;
            keep
        });
        self.write_registry(&reg).with_context(context)?;
        Ok(outcomes)
    }

    /// The registry lock file, shared by every process using this data directory.
    fn registry_lock_file(&self) -> anyhow::Result<fs::File> {
        let path = self.config.data_dir().join("registry.lock");
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))
    }

    /// Takes the registry lock, across this process's threads and other processes, for a
    /// read-modify-write update of `registry.json`.
    fn lock_registry(&self) -> anyhow::Result<RegistryLock<'_>> {
        let thread = self
            .registry_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let process = self.registry_lock_file()?;
        process.lock().context("locking the registry")?;
        Ok(RegistryLock {
            _thread: thread,
            _process: process,
        })
    }

    /// Takes the registry lock shared, as [`Self::load`] needs it.
    fn share_registry(&self) -> anyhow::Result<RegistryShare> {
        let file = self.registry_lock_file()?;
        file.lock_shared().context("locking the registry")?;
        Ok(RegistryShare { _file: file })
    }

    /// Stops indexing `root` (see [`WorktreeIndex::retire`]) and deletes its index; `closed` are
    /// the open handles closed for it. Must not be called from async context.
    fn delete_index(&self, root: &Path, closed: &[Arc<WorktreeIndex>]) -> anyhow::Result<()> {
        for w in closed.iter().filter(|w| w.root == root) {
            w.retire();
        }
        crate::index::delete(&crate::index::index_dir(&self.config, root), root)
    }

    fn write_registry(&self, reg: &RegistryFile) -> anyhow::Result<()> {
        let tmp = self.registry_path().with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(reg)?)?;
        fs::rename(tmp, self.registry_path())?;
        Ok(())
    }

    fn add(&self, path: &Path, auto: bool) -> anyhow::Result<Arc<WorktreeIndex>> {
        let root = canonicalize(path)?;
        anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
        {
            // Released before the index is opened, which takes it shared.
            let _write = self.lock_registry()?;
            let mut reg = self.registry_file()?;
            match reg.repos.iter_mut().find(|r| r.root == root) {
                None => {
                    let name = unique_name(&reg, &base_name(&reg, &root), &root);
                    reg.repos.push(RepoEntry {
                        root: root.clone(),
                        name,
                        auto,
                        removing: false,
                    });
                    self.write_registry(&reg)?;
                }
                Some(e) => {
                    // Registered by hand after being registered automatically: keep it. One
                    // whose removal stopped part-way has it finished, then is registered again.
                    if e.removing {
                        self.delete_index(&root, &[])?;
                    }
                    let changed = e.removing || (!auto && e.auto);
                    e.removing = false;
                    e.auto &= auto;
                    if changed {
                        self.write_registry(&reg)?;
                    }
                }
            }
        }
        self.open_canonical(&root)
    }

    /// Index handle for an already-canonical root, opened once per process. A handle whose index
    /// another process deleted (`lode --local unregister`), and maybe created again, is replaced
    /// and abandoned (see [`WorktreeIndex::is_current`]).
    pub fn open_canonical(&self, root: &Path) -> anyhow::Result<Arc<WorktreeIndex>> {
        if let Some(w) = self
            .worktrees
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .get(root)
            .filter(|w| w.is_current())
        {
            return Ok(w.clone());
        }
        let shared = self.share_registry()?;
        let mut map = self
            .worktrees
            .write()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        if let Some(w) = map.get(root) {
            if w.is_current() {
                return Ok(w.clone());
            }
            w.abandon();
            map.remove(root);
        }
        let w = Arc::new(self.load(root, &shared)?);
        map.insert(root.to_path_buf(), w.clone());
        Ok(w)
    }

    /// Opens the index of `root`, which must be registered. The registry lock is held shared, so
    /// no process removes it meanwhile, which would have this create it again.
    fn load(&self, root: &Path, _shared: &RegistryShare) -> anyhow::Result<WorktreeIndex> {
        anyhow::ensure!(
            self.registry_file()?.live().any(|r| r.root == root),
            "{} is not a registered worktree",
            root.display()
        );
        WorktreeIndex::open(root, &self.config, &self.model_id, self.dim)
    }

    /// The open index handle for a canonical root, else one loaded for this call only.
    fn peek(&self, root: &Path) -> anyhow::Result<Arc<WorktreeIndex>> {
        let open = self
            .worktrees
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .get(root)
            .filter(|w| w.is_current())
            .cloned();
        match open {
            Some(w) => Ok(w),
            None => self.load(root, &self.share_registry()?).map(Arc::new),
        }
    }

    /// Closes every open index that is neither in `keep` nor in use (watched, indexing or
    /// serving a request); the next use loads it again.
    pub fn close_unused(&self, keep: &BTreeSet<PathBuf>) {
        if let Ok(mut open) = self.worktrees.write() {
            open.retain(|root, w| keep.contains(root) || Arc::strong_count(w) > 1);
        }
    }

    /// Resolves the `repo` argument: a registered name, a path (canonicalized here), or — when
    /// absent — the only registered repo.
    pub fn resolve(&self, repo: Option<&str>) -> anyhow::Result<Arc<WorktreeIndex>> {
        let mut reg = self.registry_file()?;
        reg.repos.retain(|r| !r.removing);
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
                self.worktree_for(&canonicalize(Path::new(r))?)
            }
        }
    }

    pub fn repo_name(&self, root: &Path) -> String {
        list_registry(&self.registry_path())
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
            paths: req.paths.clone(),
            language: req.language.clone(),
            kinds: req.kinds.clone(),
            include_tests: req.include_tests,
        };
        let top_k = req.top_k.clamp(1, 50);
        let reranker = self.reranker();
        if let Some(e) = self.reranker_error() {
            notes.push(format!("reranker unavailable ({e}); results not reranked"));
        }
        let mut search_config = self.config.search.clone();
        if reranker.is_none() {
            search_config.doc_weight = 0.0;
        }
        if query_vec.is_none() {
            // The lexical results promised above, even where the config weights them 0.
            for w in [
                &mut search_config.lexical_weight,
                &mut search_config.identifier_lexical_weight,
            ] {
                if *w <= 0.0 {
                    *w = 1.0;
                }
            }
        }
        let mut outcome = search::search(
            &snap,
            &w.lexical(),
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
        let mut stale_reasons = w.stale_reasons(&snap);
        if w.is_writing() {
            stale_reasons.push("indexing in progress".into());
        }
        Ok(SearchResponse {
            repo: w.root.to_string_lossy().to_string(),
            stale: !stale_reasons.is_empty(),
            stale_reasons,
            notes,
            results,
            matching: Vec::new(),
            took_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    }

    pub fn status(&self, repo: Option<&str>) -> anyhow::Result<StatusReport> {
        Ok(self.status_of(&*self.resolve(repo)?))
    }

    /// Status of every registered repository; one that cannot be read is listed as failed.
    pub fn status_all(&self) -> StatusOverview {
        let mut repos = Vec::new();
        let mut failed = Vec::new();
        for e in list_registry(&self.registry_path()).repos {
            match self.peek(&e.root) {
                Ok(w) => repos.push(self.status_of(&w)),
                Err(err) => failed.push(RepoFailure {
                    name: e.name,
                    error: format!("{err:#}"),
                }),
            }
        }
        StatusOverview::new(repos, failed)
    }

    fn status_of(&self, w: &WorktreeIndex) -> StatusReport {
        let snap = w.snapshot();
        let languages = self.languages.clone();
        let pending = w.pending_files(&|p: &Path| languages.accepts(p));
        let mut stale_reasons = w.stale_reasons(&snap);
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
        StatusReport {
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
        }
    }

    pub fn worktrees(&self) -> Vec<Arc<WorktreeIndex>> {
        self.worktrees
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
}
