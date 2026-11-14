//! Per-worktree index: manifest, items, vectors and lexical state, published to readers as
//! immutable snapshots. One async mutex serializes writers; readers never block on it.

pub mod items;
pub mod lexical;
pub mod manifest;
pub mod vectors;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use arc_swap::ArcSwap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tantivy::{IndexWriter, Searcher};

use crate::cache::{CacheKey, EmbeddingCache, cache_key};
use crate::chunk::Chunker;
use crate::config::{Config, Precision};
use crate::embed::{Embedder, InputKind};
use crate::graph::ProjectGraph;
pub use items::{ItemRow, ItemStore};
pub use lexical::{Lexical, TokenizerKind};
pub use manifest::{FileEntry, Manifest};
pub use vectors::VectorIndex;

/// Files larger than this are assumed generated and skipped.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Files per indexing batch; the manifest is written after each batch.
const FILES_PER_BATCH: usize = 128;
/// Texts per embedding call (progress granularity).
const EMBED_GROUP: usize = 512;
const VECTORS_FILE: &str = "vectors.usearch";
const DOC_VECTORS_FILE: &str = "doc-vectors.usearch";

/// Worktree id: `blake3(canonical_root)`.
/// Where a worktree's index lives: `<data dir>/worktrees/<worktree id>`.
pub fn index_dir(config: &Config, canonical_root: &Path) -> PathBuf {
    config
        .data_dir()
        .join("worktrees")
        .join(worktree_id(canonical_root))
}

pub fn worktree_id(canonical_root: &Path) -> String {
    blake3::hash(canonical_root.to_string_lossy().as_bytes()).to_hex()[..16].to_string()
}

/// Everything an indexing run needs besides the index itself.
pub struct IndexContext {
    pub chunker: Chunker,
    pub embedder: Arc<dyn Embedder>,
    pub cache: Arc<EmbeddingCache>,
    pub config: Config,
}

/// Immutable searchable state. A write builds the next one and swaps it in.
pub struct Snapshot {
    pub items: Arc<HashMap<u64, Arc<ItemRow>>>,
    pub vectors: Arc<VectorIndex>,
    /// Doc-comment vectors keyed by item id; empty unless `doc_vectors` is on.
    pub doc_vectors: Arc<VectorIndex>,
    pub searcher: Searcher,
    pub graph: Arc<ProjectGraph>,
    pub model_id: String,
    pub files_indexed: usize,
    pub updated_at: u64,
    /// Why results may be out of date; empty when fresh.
    pub stale: Vec<String>,
    pub version: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    pub running: bool,
    pub files_done: usize,
    pub files_total: usize,
    pub chunks_embedded: usize,
    pub chunks_cached: usize,
    pub eta_secs: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct IndexOptions {
    pub force: bool,
    /// Only these repo-relative paths (watcher events); `None` means a full walk.
    pub paths: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexReport {
    pub files_scanned: usize,
    pub files_changed: usize,
    pub files_removed: usize,
    pub items: usize,
    pub chunks_embedded: usize,
    pub chunks_cached: usize,
    pub secs: f64,
}

struct WriterState {
    items: ItemStore,
    writer: IndexWriter,
    manifest: Manifest,
    next_id: u64,
}

pub struct WorktreeIndex {
    pub root: PathBuf,
    pub id: String,
    pub dir: PathBuf,
    model_id: String,
    dim: usize,
    precision: Precision,
    doc_vectors: bool,
    lexical: Lexical,
    snapshot: ArcSwap<Snapshot>,
    writer: tokio::sync::Mutex<Option<WriterState>>,
    /// Whether a writer has checked consistency (see `repair`) since this index was opened.
    repaired: AtomicBool,
    progress: Mutex<Progress>,
    last_report: Mutex<Option<IndexReport>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn mtime_nanos(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Walks `root` respecting ignore files (`.gitignore` at every level, `.git/info/exclude`, the
/// global git excludes file, `.ignore`, `.lodestoneignore`) and skipping hidden files, cache
/// directories (`CACHEDIR.TAG`) and symlinks. Returns (accepted rel paths, skipped files per
/// extension).
pub fn walk(
    root: &Path,
    accepts: &(dyn Fn(&Path) -> bool + Sync),
) -> (Vec<String>, BTreeMap<String, u64>) {
    walk_within(root, accepts, |_, _| true)
}

/// The subset of `paths` (relative to `root`) that [`walk`] accepts. Walks with the same rules,
/// entering only the directories that lead to `paths`.
fn walk_paths(
    root: &Path,
    accepts: &(dyn Fn(&Path) -> bool + Sync),
    paths: &[String],
) -> Vec<String> {
    let files: HashSet<PathBuf> = paths.iter().map(|p| root.join(p)).collect();
    let dirs: HashSet<PathBuf> = files
        .iter()
        .flat_map(|f| f.ancestors().skip(1).take_while(|d| d.starts_with(root)))
        .map(Path::to_path_buf)
        .collect();
    walk_within(root, accepts, move |path, is_dir| {
        if is_dir {
            dirs.contains(path)
        } else {
            files.contains(path)
        }
    })
    .0
}

/// [`walk`], also pruning entries `scope(path, is_dir)` rejects.
fn walk_within(
    root: &Path,
    accepts: &(dyn Fn(&Path) -> bool + Sync),
    scope: impl Fn(&Path, bool) -> bool + Send + Sync + 'static,
) -> (Vec<String>, BTreeMap<String, u64>) {
    let mut files = Vec::new();
    let mut skipped = BTreeMap::new();
    let walker = ignore::WalkBuilder::new(root)
        .add_custom_ignore_filename(".lodestoneignore")
        .require_git(false)
        .filter_entry(move |e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            scope(e.path(), is_dir) && !(is_dir && e.path().join("CACHEDIR.TAG").exists())
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let too_big = entry
            .metadata()
            .map(|m| m.len() > MAX_FILE_BYTES)
            .unwrap_or(true);
        if accepts(path) && !too_big {
            files.push(rel);
        } else {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_else(|| "(none)".into());
            *skipped.entry(ext).or_insert(0) += 1;
        }
    }
    files.sort();
    (files, skipped)
}

impl WorktreeIndex {
    /// Opens (or creates) the index for an already-canonicalized root. Loading never indexes;
    /// inconsistencies are reported as staleness and repaired by the next writer.
    pub fn open(
        canonical_root: &Path,
        config: &Config,
        model_id: &str,
        dim: usize,
    ) -> anyhow::Result<Self> {
        let id = worktree_id(canonical_root);
        let dir = index_dir(config, canonical_root);
        fs::create_dir_all(&dir)?;
        let manifest = Manifest::load(&dir.join("manifest.json"))?;
        let store = ItemStore::open(&dir.join("items.sqlite"))?;
        let rows = store.load_all()?;
        let precision = config.vector_precision;
        let mut stale = Vec::new();
        let index_model = manifest
            .as_ref()
            .map(|m| m.model_id.clone())
            .unwrap_or_default();
        let same_model = index_model.is_empty() || index_model == model_id;
        let load = |file: &str, enabled: bool| -> anyhow::Result<VectorIndex> {
            let loaded = if enabled && same_model {
                VectorIndex::load(&dir.join(file), dim, precision)?
            } else {
                None
            };
            loaded.map_or_else(|| VectorIndex::new(dim, precision), Ok)
        };
        let vectors = load(VECTORS_FILE, true)?;
        let doc_vectors = load(DOC_VECTORS_FILE, config.doc_vectors)?;
        match &manifest {
            None => stale.push(
                "not indexed yet (a session or search starts it; `lode index <path>` runs it now)"
                    .to_string(),
            ),
            Some(m) if m.model_id != model_id => stale.push(format!(
                "index built with model {} but {model_id} is configured; run `lode index --force`",
                m.model_id
            )),
            _ => {}
        }
        let missing = rows
            .iter()
            .filter(|r| {
                !vectors.contains(r.id)
                    || config.doc_vectors
                        && !r.item.doc.trim().is_empty()
                        && !doc_vectors.contains(r.id)
            })
            .count();
        if missing > 0 && index_model == model_id {
            stale.push(format!(
                "{missing} items have no vector yet (repaired on next index)"
            ));
        }
        let lexical_dir = match config.lexical_tokenizer {
            TokenizerKind::Code => dir.join("lexical"),
            TokenizerKind::Default => dir.join("lexical-default"),
        };
        let lexical = Lexical::open(&lexical_dir, config.lexical_tokenizer)?;
        let searcher = lexical.searcher();
        if searcher.num_docs() as usize != rows.len() {
            stale.push("lexical index out of sync with items (repaired on next index)".to_string());
        }
        let graph = fs::read(dir.join("graph.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| ProjectGraph::build(canonical_root));
        let snap = Snapshot {
            items: Arc::new(rows.into_iter().map(|r| (r.id, Arc::new(r))).collect()),
            vectors: Arc::new(vectors),
            doc_vectors: Arc::new(doc_vectors),
            searcher,
            graph: Arc::new(graph),
            model_id: if index_model.is_empty() {
                model_id.to_string()
            } else {
                index_model
            },
            files_indexed: manifest.as_ref().map(|m| m.files.len()).unwrap_or(0),
            updated_at: manifest.as_ref().map(|m| m.updated_at).unwrap_or(0),
            stale,
            version: 0,
        };
        Ok(Self {
            root: canonical_root.to_path_buf(),
            id,
            dir,
            model_id: model_id.to_string(),
            dim,
            precision,
            doc_vectors: config.doc_vectors,
            lexical,
            snapshot: ArcSwap::from_pointee(snap),
            writer: tokio::sync::Mutex::new(None),
            repaired: AtomicBool::new(false),
            progress: Mutex::new(Progress::default()),
            last_report: Mutex::new(None),
        })
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    pub fn lexical(&self) -> &Lexical {
        &self.lexical
    }

    pub fn progress(&self) -> Progress {
        self.progress.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn last_report(&self) -> Option<IndexReport> {
        self.last_report.lock().ok().and_then(|r| r.clone())
    }

    pub fn manifest(&self) -> Option<Manifest> {
        Manifest::load(&self.dir.join("manifest.json"))
            .ok()
            .flatten()
    }

    /// Waits up to `timeout` while a worktree that has never finished an index gets its first
    /// one, giving up early if indexing runs and stops without finishing it (it failed).
    pub fn await_first_index(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut started = false;
        while self.snapshot().updated_at == 0 && Instant::now() < deadline {
            let running = self.progress().running;
            if started && !running {
                break;
            }
            started |= running;
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn is_writing(&self) -> bool {
        self.writer.try_lock().is_err()
    }

    /// Drops the writer (lexical writer threads and buffers, items connection) unless a pass is
    /// running; the next pass opens it again.
    pub fn release_writer(&self) {
        if let Ok(mut writer) = self.writer.try_lock() {
            *writer = None;
        }
    }

    /// Marks the current snapshot stale (e.g. watcher saw changes not yet indexed).
    pub fn set_pending(&self, reason: Option<String>) {
        let cur = self.snapshot.load_full();
        let mut stale: Vec<String> = cur
            .stale
            .iter()
            .filter(|s| !s.starts_with("pending:"))
            .cloned()
            .collect();
        if let Some(r) = reason {
            stale.push(format!("pending: {r}"));
        }
        if stale != cur.stale {
            self.publish_from(
                &cur,
                cur.items.clone(),
                cur.vectors.clone(),
                cur.doc_vectors.clone(),
                cur.searcher.clone(),
                cur.graph.clone(),
                cur.files_indexed,
                cur.updated_at,
                stale,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_from(
        &self,
        cur: &Snapshot,
        items: Arc<HashMap<u64, Arc<ItemRow>>>,
        vectors: Arc<VectorIndex>,
        doc_vectors: Arc<VectorIndex>,
        searcher: Searcher,
        graph: Arc<ProjectGraph>,
        files_indexed: usize,
        updated_at: u64,
        stale: Vec<String>,
    ) {
        self.snapshot.store(Arc::new(Snapshot {
            items,
            vectors,
            doc_vectors,
            searcher,
            graph,
            model_id: self.model_id.clone(),
            files_indexed,
            updated_at,
            stale,
            version: cur.version + 1,
        }));
    }

    /// Async entry point: waits for the writer lock without blocking the runtime.
    pub async fn index(
        self: &Arc<Self>,
        ctx: Arc<IndexContext>,
        opts: IndexOptions,
    ) -> anyhow::Result<IndexReport> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.index_blocking(&ctx, opts)).await?
    }

    /// Runs one indexing pass under the writer lock. Must not be called from async context.
    pub fn index_blocking(
        &self,
        ctx: &IndexContext,
        opts: IndexOptions,
    ) -> anyhow::Result<IndexReport> {
        let mut guard = self.writer.blocking_lock();
        if guard.is_none() {
            let items = ItemStore::open(&self.dir.join("items.sqlite"))?;
            let next_id = items.max_id()? + 1;
            *guard = Some(WriterState {
                items,
                writer: self.lexical.writer().context("acquiring lexical writer")?,
                manifest: self.manifest().unwrap_or_else(|| Manifest {
                    root: self.root.clone(),
                    model_id: self.model_id.clone(),
                    ..Manifest::default()
                }),
                next_id,
            });
        }
        let st = guard.as_mut().expect("writer state initialized");
        let result = self.run(st, ctx, opts);
        if let (Ok(r), Ok(mut last)) = (&result, self.last_report.lock()) {
            *last = Some(r.clone());
        }
        if let Ok(mut p) = self.progress.lock() {
            p.running = false;
            p.eta_secs = None;
        }
        result
    }

    fn set_progress(&self, f: impl FnOnce(&mut Progress)) {
        if let Ok(mut p) = self.progress.lock() {
            f(&mut p);
        }
    }

    /// Restores consistency after a crash: vectors missing from usearch come back from the
    /// cache, lexical is rebuilt if its doc count disagrees, and manifest entries whose items
    /// are incomplete are dropped so the file is re-indexed.
    fn repair(&self, st: &mut WriterState, ctx: &IndexContext) -> anyhow::Result<()> {
        let snap = self.snapshot();
        let mut vectors = snap.vectors.duplicate()?;
        let mut doc_vectors = snap.doc_vectors.duplicate()?;
        let mut drop_paths: HashSet<String> = HashSet::new();
        let code_missing: Vec<(&ItemRow, CacheKey)> = snap
            .items
            .values()
            .filter(|r| !vectors.contains(r.id))
            .map(|r| (r.as_ref(), r.chunk_key))
            .collect();
        self.restore_from_cache(ctx, &mut vectors, &code_missing, &mut drop_paths)?;
        let doc_missing: Vec<(&ItemRow, CacheKey)> = snap
            .items
            .values()
            .filter(|r| !doc_vectors.contains(r.id))
            .filter_map(|r| self.doc_entry(r).map(|(k, _)| (r.as_ref(), k)))
            .collect();
        self.restore_from_cache(ctx, &mut doc_vectors, &doc_missing, &mut drop_paths)?;
        let searcher = self.lexical.searcher();
        if searcher.num_docs() as usize != snap.items.len() {
            st.writer.delete_all_documents()?;
            for r in snap.items.values() {
                self.lexical.add(&st.writer, r)?;
            }
            st.writer.commit()?;
            self.lexical.reload()?;
        }
        let present: HashSet<u64> = snap.items.keys().copied().collect();
        st.manifest.files.retain(|path, e| {
            !drop_paths.contains(path) && e.chunk_ids.iter().all(|id| present.contains(id))
        });
        st.manifest.save(&self.dir.join("manifest.json"))?;
        vectors.save(&self.dir.join(VECTORS_FILE))?;
        doc_vectors.save(&self.dir.join(DOC_VECTORS_FILE))?;
        let stale = snap
            .stale
            .iter()
            .filter(|s| s.starts_with("pending:"))
            .cloned()
            .collect();
        self.publish_from(
            &snap,
            snap.items.clone(),
            Arc::new(vectors),
            Arc::new(doc_vectors),
            self.lexical.searcher(),
            snap.graph.clone(),
            st.manifest.files.len(),
            snap.updated_at,
            stale,
        );
        self.repaired.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn run(
        &self,
        st: &mut WriterState,
        ctx: &IndexContext,
        opts: IndexOptions,
    ) -> anyhow::Result<IndexReport> {
        let started = Instant::now();
        let model_id = ctx.embedder.model_id().to_string();
        anyhow::ensure!(
            model_id == self.model_id,
            "index opened for {} but embedder is {model_id}",
            self.model_id
        );
        if st.manifest.model_id != model_id || opts.force {
            // Different model (or forced): vectors are incompatible, start over.
            st.items.clear()?;
            st.writer.delete_all_documents()?;
            st.writer.commit()?;
            self.lexical.reload()?;
            st.manifest = Manifest {
                root: self.root.clone(),
                model_id: model_id.clone(),
                ..Manifest::default()
            };
            let snap = self.snapshot();
            self.publish_from(
                &snap,
                Arc::new(HashMap::new()),
                Arc::new(VectorIndex::new(self.dim, self.precision)?),
                Arc::new(VectorIndex::new(self.dim, self.precision)?),
                self.lexical.searcher(),
                snap.graph.clone(),
                0,
                0,
                vec!["indexing from scratch".into()],
            );
            self.repaired.store(true, Ordering::Relaxed);
        }
        if !self.repaired.load(Ordering::Relaxed) {
            self.repair(st, ctx)?;
        }
        ctx.cache.register_model(&model_id, self.dim, None)?;
        let chunker_fp = ctx.chunker.fingerprint();
        // Chunking settings or queries changed: re-chunk everything (embeddings stay cached).
        let rechunk = opts.paths.is_none() && st.manifest.chunker != chunker_fp;
        let graph = Arc::new(ProjectGraph::build(&self.root));
        fs::write(self.dir.join("graph.json"), serde_json::to_vec(&*graph)?)?;
        let registry = ctx.chunker.registry().clone();
        let accepts = |p: &Path| registry.accepts(p);

        // Decide which files changed.
        let candidates: Vec<String> = match &opts.paths {
            None => {
                let (files, skipped) = walk(&self.root, &accepts);
                st.manifest.skipped_extensions = skipped;
                files
            }
            // Watcher events: only files a full walk would index.
            Some(paths) => walk_paths(&self.root, &accepts, paths),
        };
        let candidate_set: HashSet<&str> = candidates.iter().map(String::as_str).collect();
        let mut removed: Vec<String> = match &opts.paths {
            None => {
                let snap = self.snapshot();
                let mut known: HashSet<String> = st.manifest.files.keys().cloned().collect();
                known.extend(snap.items.values().map(|r| r.item.rel_path.clone()));
                known
                    .into_iter()
                    .filter(|p| !candidate_set.contains(p.as_str()))
                    .collect()
            }
            Some(paths) => paths
                .iter()
                .filter(|p| !candidate_set.contains(p.as_str()))
                .cloned()
                .collect(),
        };
        removed.sort();

        let stat: Vec<(String, Option<(u64, i64)>)> = candidates
            .par_iter()
            .map(|rel| {
                let meta = fs::metadata(self.root.join(rel)).ok();
                (rel.clone(), meta.map(|m| (m.len(), mtime_nanos(&m))))
            })
            .collect();
        let mut changed: Vec<(String, u64, i64)> = Vec::new();
        let snap = self.snapshot();
        let indexed_paths: HashSet<&str> = snap
            .items
            .values()
            .map(|r| r.item.rel_path.as_str())
            .collect();
        for (rel, meta) in stat {
            let Some((size, mtime)) = meta else { continue };
            match st.manifest.files.get(&rel) {
                Some(e)
                    if !rechunk
                        && e.size == size
                        && e.mtime == mtime
                        && indexed_paths.contains(rel.as_str()) == !e.chunk_ids.is_empty() => {}
                _ => changed.push((rel, size, mtime)),
            }
        }
        drop(indexed_paths);
        drop(snap);
        let mut report = IndexReport {
            files_scanned: candidates.len(),
            files_removed: removed.len(),
            ..IndexReport::default()
        };
        self.set_progress(|p| {
            *p = Progress {
                running: true,
                files_total: changed.len(),
                ..Progress::default()
            };
        });

        if !removed.is_empty() {
            self.commit_batch(st, &removed, Vec::new(), &[], &[], &graph)?;
            for p in &removed {
                st.manifest.files.remove(p);
            }
            st.manifest.save(&self.dir.join("manifest.json"))?;
        }

        for batch in changed.chunks(FILES_PER_BATCH) {
            // Read + hash; files whose content is unchanged only get their stat refreshed.
            let read: Vec<(String, u64, i64, String, Vec<u8>)> = batch
                .par_iter()
                .filter_map(|(rel, size, mtime)| {
                    let bytes = fs::read(self.root.join(rel)).ok()?;
                    let hash = blake3::hash(&bytes).to_hex().to_string();
                    Some((rel.clone(), *size, *mtime, hash, bytes))
                })
                .collect();
            let snap = self.snapshot();
            let indexed_paths: HashSet<&str> = snap
                .items
                .values()
                .map(|r| r.item.rel_path.as_str())
                .collect();
            let mut to_index = Vec::new();
            for (rel, size, mtime, hash, bytes) in read {
                match st.manifest.files.get_mut(&rel) {
                    Some(e)
                        if !rechunk
                            && e.file_hash == hash
                            && (indexed_paths.contains(rel.as_str()) || e.chunk_ids.is_empty()) =>
                    {
                        e.size = size;
                        e.mtime = mtime;
                    }
                    _ => to_index.push((rel, size, mtime, hash, bytes)),
                }
            }
            drop(indexed_paths);
            drop(snap);
            let chunked: Vec<(String, u64, i64, String, Vec<crate::chunk::Item>)> = to_index
                .into_par_iter()
                .map(|(rel, size, mtime, hash, bytes)| {
                    let items = ctx.chunker.chunk(&rel, &bytes);
                    (rel, size, mtime, hash, items)
                })
                .collect();
            let mut rows: Vec<ItemRow> = Vec::new();
            for (rel, _, _, _, items) in &chunked {
                for item in items {
                    rows.push(ItemRow {
                        id: st.next_id,
                        unit: graph.unit_of(rel).map(String::from),
                        chunk_key: cache_key(&model_id, &item.text),
                        item: item.clone(),
                    });
                    st.next_id += 1;
                }
            }
            let code: Vec<(CacheKey, &str)> = rows
                .iter()
                .map(|r| (r.chunk_key, r.item.text.as_str()))
                .collect();
            let vectors = self.embed_cached(ctx, &code, &mut report)?;
            let docs: Vec<(u64, CacheKey, String)> = rows
                .iter()
                .filter_map(|r| self.doc_entry(r).map(|(k, t)| (r.id, k, t)))
                .collect();
            let doc_entries: Vec<(CacheKey, &str)> =
                docs.iter().map(|(_, k, t)| (*k, t.as_str())).collect();
            let doc_vectors: Vec<(u64, Vec<f32>)> = docs
                .iter()
                .map(|d| d.0)
                .zip(self.embed_cached(ctx, &doc_entries, &mut report)?)
                .collect();
            let paths: Vec<String> = chunked.iter().map(|c| c.0.clone()).collect();
            self.commit_batch(st, &paths, rows.clone(), &vectors, &doc_vectors, &graph)?;
            for (rel, size, mtime, hash, _) in chunked {
                let ids = rows
                    .iter()
                    .filter(|r| r.item.rel_path == rel)
                    .map(|r| r.id)
                    .collect();
                st.manifest.files.insert(
                    rel,
                    FileEntry {
                        file_hash: hash,
                        mtime,
                        size,
                        chunk_ids: ids,
                    },
                );
            }
            report.files_changed += paths.len();
            report.items += rows.len();
            st.manifest.save(&self.dir.join("manifest.json"))?;
            let elapsed = started.elapsed().as_secs_f64();
            self.set_progress(|p| {
                p.files_done += batch.len();
                let left = p.files_total.saturating_sub(p.files_done);
                p.eta_secs = (p.files_done > 0)
                    .then(|| (elapsed / p.files_done as f64 * left as f64) as u64);
            });
        }
        if opts.paths.is_none() {
            st.manifest.chunker = chunker_fp;
        }
        st.manifest.updated_at = now_secs();
        st.manifest.save(&self.dir.join("manifest.json"))?;
        let snap = self.snapshot();
        if report.files_changed > 0 || report.files_removed > 0 || !snap.stale.is_empty() {
            snap.vectors.save(&self.dir.join(VECTORS_FILE))?;
            snap.doc_vectors.save(&self.dir.join(DOC_VECTORS_FILE))?;
        }
        let stale = snap
            .stale
            .iter()
            .filter(|s| s.starts_with("pending:"))
            .cloned()
            .collect();
        self.publish_from(
            &snap,
            snap.items.clone(),
            snap.vectors.clone(),
            snap.doc_vectors.clone(),
            snap.searcher.clone(),
            graph,
            st.manifest.files.len(),
            st.manifest.updated_at,
            stale,
        );
        report.secs = started.elapsed().as_secs_f64();
        Ok(report)
    }

    /// The doc vector's cache key and text for `row`, when doc vectors are on and it has a doc.
    fn doc_entry(&self, row: &ItemRow) -> Option<(CacheKey, String)> {
        if !self.doc_vectors {
            return None;
        }
        let text = row.item.doc_text()?;
        Some((cache_key(&self.model_id, &text), text))
    }

    /// Adds cached vectors for `missing` to `index`; paths whose vector is not cached are
    /// collected in `drop_paths` so the next pass re-indexes them.
    fn restore_from_cache(
        &self,
        ctx: &IndexContext,
        index: &mut VectorIndex,
        missing: &[(&ItemRow, CacheKey)],
        drop_paths: &mut HashSet<String>,
    ) -> anyhow::Result<()> {
        if missing.is_empty() {
            return Ok(());
        }
        let keys: Vec<CacheKey> = missing.iter().map(|(_, k)| *k).collect();
        let cached = ctx.cache.get_many(&keys)?;
        let mut add = Vec::new();
        for (r, k) in missing {
            match cached.get(k) {
                Some(v) if v.len() == self.dim => add.push((r.id, v.as_slice())),
                _ => {
                    drop_paths.insert(r.item.rel_path.clone());
                }
            }
        }
        index.add_many(&add)
    }

    /// Vectors for `(cache key, text)` entries: cache lookups, then embedding for misses
    /// (written back to the cache).
    fn embed_cached(
        &self,
        ctx: &IndexContext,
        entries: &[(CacheKey, &str)],
        report: &mut IndexReport,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let keys: Vec<CacheKey> = entries.iter().map(|(k, _)| *k).collect();
        let mut cached = ctx.cache.get_many(&keys)?;
        let mut miss_idx: Vec<usize> = Vec::new();
        let mut seen = HashSet::new();
        for (i, (k, _)) in entries.iter().enumerate() {
            if !cached.contains_key(k) && seen.insert(*k) {
                miss_idx.push(i);
            }
        }
        let hits = entries
            .iter()
            .filter(|(k, _)| cached.contains_key(k))
            .count();
        report.chunks_cached += hits;
        self.set_progress(|p| p.chunks_cached += hits);
        for group in miss_idx.chunks(EMBED_GROUP) {
            let texts: Vec<&str> = group.iter().map(|&i| entries[i].1).collect();
            let vecs = ctx.embedder.embed(&texts, InputKind::Document)?;
            let put: Vec<_> = group
                .iter()
                .zip(&vecs)
                .map(|(&i, v)| (entries[i].0, v.as_slice()))
                .collect();
            ctx.cache.put_many(&self.model_id, &put)?;
            for (&i, v) in group.iter().zip(vecs) {
                cached.insert(entries[i].0, v);
            }
            report.chunks_embedded += group.len();
            self.set_progress(|p| p.chunks_embedded += group.len());
        }
        keys.iter()
            .map(|k| {
                cached
                    .get(k)
                    .cloned()
                    .context("vector missing after embedding")
            })
            .collect()
    }

    /// Replaces the items of `paths` with `rows` in every store, then publishes a snapshot.
    /// Order: items.sqlite, lexical, vectors (in memory); the caller writes the manifest last.
    fn commit_batch(
        &self,
        st: &mut WriterState,
        paths: &[String],
        rows: Vec<ItemRow>,
        vectors: &[Vec<f32>],
        doc_vectors: &[(u64, Vec<f32>)],
        graph: &Arc<ProjectGraph>,
    ) -> anyhow::Result<()> {
        let snap = self.snapshot();
        let old_ids = st.items.ids_for_paths(paths)?;
        st.items.replace_files(paths, &rows)?;
        self.lexical.delete_paths(&st.writer, paths);
        for r in &rows {
            self.lexical.add(&st.writer, r)?;
        }
        st.writer.commit()?;
        self.lexical.reload()?;
        let mut vec_index = snap.vectors.duplicate()?;
        vec_index.remove_many(&old_ids);
        let entries: Vec<(u64, &[f32])> = rows
            .iter()
            .zip(vectors)
            .map(|(r, v)| (r.id, v.as_slice()))
            .collect();
        vec_index.add_many(&entries)?;
        let mut doc_index = snap.doc_vectors.duplicate()?;
        doc_index.remove_many(&old_ids);
        let doc_entries: Vec<(u64, &[f32])> = doc_vectors
            .iter()
            .map(|(id, v)| (*id, v.as_slice()))
            .collect();
        doc_index.add_many(&doc_entries)?;
        let mut items = (*snap.items).clone();
        for id in &old_ids {
            items.remove(id);
        }
        for r in rows {
            items.insert(r.id, Arc::new(r));
        }
        let stale = snap.stale.clone();
        self.publish_from(
            &snap,
            Arc::new(items),
            Arc::new(vec_index),
            Arc::new(doc_index),
            self.lexical.searcher(),
            graph.clone(),
            st.manifest.files.len(),
            snap.updated_at,
            stale,
        );
        Ok(())
    }

    /// Per-extension accounting for `status`: (tagged chunks, text chunks, skipped files).
    pub fn extension_counts(&self) -> BTreeMap<String, (u64, u64, u64)> {
        let snap = self.snapshot();
        let mut out: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        for r in snap.items.values() {
            let ext = Path::new(&r.item.rel_path)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_else(|| "(none)".into());
            let e = out.entry(ext).or_default();
            if r.item.tagged {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
        }
        if let Some(m) = self.manifest() {
            for (ext, n) in m.skipped_extensions {
                out.entry(ext).or_default().2 += n;
            }
        }
        out
    }

    /// Files whose (size, mtime) differ from the manifest, plus deletions. Read-only.
    pub fn pending_files(&self, accepts: &(dyn Fn(&Path) -> bool + Sync)) -> usize {
        let Some(m) = self.manifest() else { return 0 };
        let (files, _) = walk(&self.root, accepts);
        let set: HashSet<&str> = files.iter().map(String::as_str).collect();
        let changed = files
            .iter()
            .filter(
                |f| match (m.files.get(*f), fs::metadata(self.root.join(f))) {
                    (Some(e), Ok(meta)) => e.size != meta.len() || e.mtime != mtime_nanos(&meta),
                    _ => true,
                },
            )
            .count();
        changed + m.files.keys().filter(|k| !set.contains(k.as_str())).count()
    }
}
