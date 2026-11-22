//! Per-worktree index: manifest, items, vectors and lexical state, published to readers as
//! immutable snapshots. One async mutex serializes writers; readers never block on it.

pub mod items;
pub mod lexical;
pub mod manifest;
pub mod vectors;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use arc_swap::ArcSwap;
use rayon::iter::Either;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tantivy::{IndexWriter, Searcher};

use crate::cache::{CacheKey, EmbeddingCache, cache_key};
use crate::checkout::{self, Checkout};
use crate::chunk::{Chunker, extension_of};
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
/// Why an index with a failed pass's work left (see [`Manifest::retry_full`]) may be out of date.
const RETRY_REASON: &str =
    "an indexing pass failed before it finished; the next index retries its files";
/// The worktree's interprocess writer lock file.
const WRITER_LOCK: &str = "writer.lock";
/// The paths of the batch being committed, until both stores have.
const BATCH_JOURNAL: &str = "batch.json";

/// A changed file read for indexing: (path, size, mtime, content hash, contents).
type ReadFile = (String, u64, i64, String, Vec<u8>);

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
    /// Whether `searcher` is in step with `items`: not while the lexical index was last kept by
    /// another tokenizer, until a writer rebuilds it. Searches leave it out until then.
    pub lexical_current: bool,
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
    /// The worktree's interprocess writer lock (see [`WRITER_LOCK`]), held while this lives.
    _lock: fs::File,
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
    tokenizer: TokenizerKind,
    lexical: Lexical,
    snapshot: ArcSwap<Snapshot>,
    writer: tokio::sync::Mutex<Option<WriterState>>,
    /// Whether a writer has checked consistency (see `repair`) since this index was opened.
    repaired: AtomicBool,
    progress: Mutex<Progress>,
    last_report: Mutex<Option<IndexReport>>,
    /// Work the watcher has queued but not indexed yet; kept apart from snapshots, which only
    /// writers publish.
    pending: Mutex<Option<String>>,
    /// The manifest generation this process last read or wrote (see [`Manifest::generation`]).
    generation: AtomicU64,
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

/// What a walk listed.
#[derive(Debug, Default)]
pub struct Walked {
    /// Accepted files, relative to the root, sorted.
    pub files: Vec<String>,
    /// Skipped files per extension.
    pub skipped: BTreeMap<String, u64>,
    /// Paths (relative to the root; `""` is the root) the walk could not read. Files below them
    /// may exist without being listed.
    pub unread: Vec<String>,
}

impl Walked {
    /// Whether `rel` is at or below a path the walk could not read.
    pub fn missed(&self, rel: &str) -> bool {
        self.unread.iter().any(|u| {
            u.is_empty()
                || rel == u
                || rel
                    .strip_prefix(u.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        })
    }
}

/// Walks `root` respecting ignore files (`.gitignore` at every level, `.git/info/exclude`, the
/// global git excludes file relative to `root`, `.ignore`, `.lodestoneignore`) and skipping
/// hidden files, cache directories (`CACHEDIR.TAG`), symlinks and nested checkouts served by
/// their own index (see [`crate::checkout::Checkout::is_separate_from`]).
pub fn walk_tree(root: &Path, accepts: &(dyn Fn(&Path) -> bool + Sync)) -> Walked {
    walk_within(root, accepts, |_, _| true)
}

/// [`walk_tree`]'s (accepted files, skipped files per extension).
pub fn walk(
    root: &Path,
    accepts: &(dyn Fn(&Path) -> bool + Sync),
) -> (Vec<String>, BTreeMap<String, u64>) {
    let walked = walk_tree(root, accepts);
    (walked.files, walked.skipped)
}

/// The subset of `paths` (relative to `root`) that [`walk_tree`] accepts. Walks with the same rules,
/// entering only the directories that lead to `paths`.
fn walk_paths(root: &Path, accepts: &(dyn Fn(&Path) -> bool + Sync), paths: &[String]) -> Walked {
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
}

/// Where an I/O error stopped `e`'s walk from reading part of the tree: its path, else `root`.
/// `None` for soft errors (an unparsable ignore-file line), which leave the listing complete.
fn unread_path(e: &ignore::Error, root: &Path) -> Option<PathBuf> {
    match e {
        ignore::Error::WithPath { path, err } => unread_path(err, root).map(|_| path.clone()),
        ignore::Error::WithDepth { err, .. } => unread_path(err, root),
        ignore::Error::Io(_) => Some(root.to_path_buf()),
        _ => None,
    }
}

/// [`walk_tree`], also pruning entries `scope(path, is_dir)` rejects.
fn walk_within(
    root: &Path,
    accepts: &(dyn Fn(&Path) -> bool + Sync),
    scope: impl Fn(&Path, bool) -> bool + Send + Sync + 'static,
) -> Walked {
    let mut out = Walked::default();
    let outer = Checkout::of(root);
    let walker = ignore::WalkBuilder::new(root)
        // Global excludes match paths relative to this directory; the default is the process's.
        .current_dir(root)
        .add_custom_ignore_filename(".lodestoneignore")
        .require_git(false)
        .filter_entry(move |e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            scope(e.path(), is_dir)
                && !(is_dir
                    && (e.path().join("CACHEDIR.TAG").exists()
                        || e.depth() > 0
                            && checkout::is_separate_checkout(e.path(), outer.as_ref())))
        })
        .build();
    let rel_of = |path: &Path| {
        path.strip_prefix(root)
            .ok()
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
    };
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                // An unreadable path outside the tree is a parent's ignore file: missing rules
                // list more files, never fewer.
                if let Some(rel) = unread_path(&e, root).and_then(|p| rel_of(&p)) {
                    out.unread.push(rel);
                }
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(rel) = rel_of(path) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            out.unread.push(rel);
            continue;
        };
        if accepts(path) && meta.len() <= MAX_FILE_BYTES {
            out.files.push(rel);
        } else {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_else(|| "(none)".into());
            *out.skipped.entry(ext).or_insert(0) += 1;
        }
    }
    out.files.sort();
    out
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
        let dir = index_dir(config, canonical_root);
        fs::create_dir_all(&dir)?;
        let lexical_dir = match config.lexical_tokenizer {
            TokenizerKind::Code => dir.join("lexical"),
            TokenizerKind::Default => dir.join("lexical-default"),
        };
        let lexical = Lexical::open(&lexical_dir, config.lexical_tokenizer)?;
        let empty = Snapshot {
            items: Arc::new(HashMap::new()),
            vectors: Arc::new(VectorIndex::new(dim, config.vector_precision)?),
            doc_vectors: Arc::new(VectorIndex::new(dim, config.vector_precision)?),
            searcher: lexical.searcher(),
            graph: Arc::new(ProjectGraph::default()),
            model_id: model_id.to_string(),
            files_indexed: 0,
            updated_at: 0,
            stale: Vec::new(),
            lexical_current: true,
            version: 0,
        };
        let w = Self {
            root: canonical_root.to_path_buf(),
            id: worktree_id(canonical_root),
            dir,
            model_id: model_id.to_string(),
            dim,
            precision: config.vector_precision,
            doc_vectors: config.doc_vectors,
            tokenizer: config.lexical_tokenizer,
            lexical,
            snapshot: ArcSwap::from_pointee(empty),
            writer: tokio::sync::Mutex::new(None),
            repaired: AtomicBool::new(false),
            progress: Mutex::new(Progress::default()),
            last_report: Mutex::new(None),
            pending: Mutex::new(None),
            generation: AtomicU64::new(0),
        };
        w.load_snapshot(0)?;
        Ok(w)
    }

    /// Publishes the index as on disk, as snapshot `version`, and records the manifest
    /// generation it was read at: only once all of it has loaded, so a failed read is read again
    /// by the next writer.
    fn load_snapshot(&self, version: u64) -> anyhow::Result<()> {
        let (snap, generation) = self.read_snapshot(version)?;
        self.snapshot.store(Arc::new(snap));
        self.generation.store(generation, Ordering::SeqCst);
        Ok(())
    }

    /// The index as on disk, as a snapshot numbered `version`, and the manifest generation it was
    /// read at. Inconsistencies are reported as staleness, for the next writer to repair.
    fn read_snapshot(&self, version: u64) -> anyhow::Result<(Snapshot, u64)> {
        let dir = &self.dir;
        let (dim, precision, model_id) = (self.dim, self.precision, self.model_id.as_str());
        let manifest = Manifest::load(&dir.join("manifest.json"))?;
        let generation = manifest.as_ref().map_or(0, |m| m.generation);
        let rows = ItemStore::open(&dir.join("items.sqlite"))?.load_all()?;
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
        let doc_vectors = load(DOC_VECTORS_FILE, self.doc_vectors)?;
        match &manifest {
            None => stale.push(
                "not indexed yet (a session or search starts it; `lode index <path>` runs it now)"
                    .to_string(),
            ),
            Some(m) if m.model_id != model_id => stale.push(format!(
                "index built with model {} but {model_id} is configured; run `lode index --force`",
                m.model_id
            )),
            Some(m) => {
                stale.extend(manifest_stale(m));
                if dir.join(BATCH_JOURNAL).exists() && !stale.iter().any(|s| s == RETRY_REASON) {
                    stale.push(RETRY_REASON.to_string());
                }
            }
        }
        let missing = rows
            .iter()
            .filter(|r| {
                !vectors.contains(r.id)
                    || self.doc_vectors
                        && !r.item.doc.trim().is_empty()
                        && !doc_vectors.contains(r.id)
            })
            .count();
        if missing > 0 && index_model == model_id {
            stale.push(format!(
                "{missing} items have no vector yet (repaired on next index)"
            ));
        }
        let ids: HashSet<u64> = rows.iter().map(|r| r.id).collect();
        if stray_count(&vectors, &ids) + stray_count(&doc_vectors, &ids) > 0 {
            stale.push("vectors of deleted items remain (repaired on next index)".to_string());
        }
        self.lexical.reload()?;
        let searcher = self.lexical.searcher();
        // Another tokenizer's lexical index was kept in step with the items meanwhile (or no
        // manifest says which was): this one may map ids to items long gone.
        let lexical_current = manifest
            .as_ref()
            .is_none_or(|m| m.lexical_tokenizer == Some(self.tokenizer));
        if !lexical_current {
            stale.push(
                "the keyword index was built with another tokenizer; keyword results are left out \
                 until the next index rebuilds it"
                    .to_string(),
            );
        } else if searcher.num_docs() as usize != rows.len() {
            stale.push("lexical index out of sync with items (repaired on next index)".to_string());
        }
        let graph = fs::read(dir.join("graph.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| ProjectGraph::build(&self.root));
        Ok((
            Snapshot {
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
                lexical_current,
                version,
            },
            generation,
        ))
    }

    /// Saves `manifest` (with `next_id` the lowest id not allocated) as a new generation.
    fn save_manifest(&self, manifest: &mut Manifest, next_id: u64) -> anyhow::Result<()> {
        manifest.generation += 1;
        manifest.next_id = manifest.next_id.max(next_id);
        manifest.save(&self.dir.join("manifest.json"))?;
        self.generation.store(manifest.generation, Ordering::SeqCst);
        Ok(())
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

    /// Records work queued but not indexed yet (e.g. watcher saw changes), or clears it.
    pub fn set_pending(&self, reason: Option<String>) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = reason;
    }

    /// Why results from `snap` may be out of date: its own reasons, then pending work.
    pub fn stale_reasons(&self, snap: &Snapshot) -> Vec<String> {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        snap.stale
            .iter()
            .cloned()
            .chain(pending.as_ref().map(|r| format!("pending: {r}")))
            .collect()
    }

    /// Takes the worktree's interprocess writer lock: one for every tokenizer, whose lexical
    /// indexes are separate while the items, manifest, vectors and journal are shared.
    fn writer_lock(&self) -> anyhow::Result<fs::File> {
        let path = self.dir.join(WRITER_LOCK);
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(fs::TryLockError::WouldBlock) => anyhow::bail!(
                "another lodestone process is indexing {}",
                self.root.display()
            ),
            Err(fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// Replaces the lexical index's entries with `items`' and publishes it, in step.
    fn rebuild_lexical(
        &self,
        st: &mut WriterState,
        items: &HashMap<u64, Arc<ItemRow>>,
    ) -> anyhow::Result<()> {
        st.writer.delete_all_documents()?;
        for r in items.values() {
            self.lexical.add(&st.writer, r)?;
        }
        st.writer.commit()?;
        self.lexical.reload()?;
        let snap = self.snapshot();
        self.snapshot.store(Arc::new(Snapshot {
            items: snap.items.clone(),
            vectors: snap.vectors.clone(),
            doc_vectors: snap.doc_vectors.clone(),
            searcher: self.lexical.searcher(),
            graph: snap.graph.clone(),
            model_id: snap.model_id.clone(),
            files_indexed: snap.files_indexed,
            updated_at: snap.updated_at,
            stale: snap.stale.clone(),
            lexical_current: true,
            version: snap.version + 1,
        }));
        Ok(())
    }

    /// Publishes the current snapshot again with `stale` as its reasons.
    fn publish_stale(&self, stale: Vec<String>) {
        let snap = self.snapshot();
        self.publish_from(
            &snap,
            snap.items.clone(),
            snap.vectors.clone(),
            snap.doc_vectors.clone(),
            snap.searcher.clone(),
            snap.graph.clone(),
            snap.files_indexed,
            snap.updated_at,
            stale,
        );
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
            lexical_current: self.snapshot.load().lexical_current,
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
            // Taken before anything a writer recovers or rewrites.
            let lock = self.writer_lock()?;
            let writer = self.lexical.writer().context("acquiring lexical writer")?;
            let mut manifest = self.manifest().unwrap_or_else(|| Manifest {
                root: self.root.clone(),
                model_id: self.model_id.clone(),
                ..Manifest::default()
            });
            if manifest.generation != self.generation.load(Ordering::SeqCst) {
                // Another process wrote the index since this one read it: read it again, and
                // check it, before anything is decided from the old view.
                self.load_snapshot(self.snapshot().version + 1)?;
                self.repaired.store(false, Ordering::Relaxed);
            }
            let items = ItemStore::open(&self.dir.join("items.sqlite"))?;
            // Ids are never reused: above every id stored, published, in the lexical index or
            // ever allocated (`Manifest::next_id`), which a failed or interrupted batch, or
            // another tokenizer's era, may have left differing.
            let published = self.snapshot().items.keys().max().copied().unwrap_or(0);
            let next_id = (items.max_id()?.max(published).max(self.lexical.max_id()?) + 1)
                .max(manifest.next_id);
            self.recover_batch(&mut manifest, next_id)?;
            *guard = Some(WriterState {
                _lock: lock,
                items,
                writer,
                manifest,
                next_id,
            });
        }
        let st = guard.as_mut().expect("writer state initialized");
        let scope = (!opts.force).then(|| opts.paths.clone()).flatten();
        let result = self.run(st, ctx, opts);
        if let (Ok(r), Ok(mut last)) = (&result, self.last_report.lock()) {
            *last = Some(r.clone());
        }
        if let Err(e) = &result {
            // What the pass did not get to (changed or removed files, a rebuild) may still be
            // served as it was: recorded, so the next pass (in any process) covers it, and
            // reported until one succeeds.
            match scope {
                Some(paths) => {
                    st.manifest.retry_paths.extend(paths);
                    st.manifest.retry_paths.sort();
                    st.manifest.retry_paths.dedup();
                }
                None => st.manifest.retry_full = true,
            }
            if let Err(e) = self.save_manifest(&mut st.manifest, st.next_id) {
                tracing::warn!("recording the failed pass: {e:#}");
            }
            let mut stale = manifest_stale(&st.manifest);
            stale.push(format!(
                "the last indexing pass failed ({e:#}); results may be out of date"
            ));
            self.publish_stale(stale);
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
    /// cache, vectors of items no longer stored are dropped, lexical is rebuilt if its doc count
    /// disagrees, and manifest entries whose items are incomplete are dropped so the file is
    /// re-indexed.
    fn repair(&self, st: &mut WriterState, ctx: &IndexContext) -> anyhow::Result<()> {
        let snap = self.snapshot();
        let mut vectors = snap.vectors.duplicate()?;
        let mut doc_vectors = snap.doc_vectors.duplicate()?;
        let ids: HashSet<u64> = snap.items.keys().copied().collect();
        if stray_count(&vectors, &ids) > 0 {
            vectors = vectors.rebuilt(&ids)?;
        }
        if stray_count(&doc_vectors, &ids) > 0 {
            doc_vectors = doc_vectors.rebuilt(&ids)?;
        }
        let mut drop_paths: HashSet<String> = HashSet::new();
        // Keyed by this index's model, not the stored key: an item kept through a model change
        // (its file unreadable) has the old model's key, and the old model's vector.
        let code_missing: Vec<(&ItemRow, CacheKey)> = snap
            .items
            .values()
            .filter(|r| !vectors.contains(r.id))
            .map(|r| (r.as_ref(), cache_key(&self.model_id, &r.item.text)))
            .collect();
        self.restore_from_cache(ctx, &mut vectors, &code_missing, &mut drop_paths)?;
        let doc_missing: Vec<(&ItemRow, CacheKey)> = snap
            .items
            .values()
            .filter(|r| !doc_vectors.contains(r.id))
            .filter_map(|r| self.doc_entry(r).map(|(k, _)| (r.as_ref(), k)))
            .collect();
        self.restore_from_cache(ctx, &mut doc_vectors, &doc_missing, &mut drop_paths)?;
        // A tokenizer change rebuilds the lexical index right after (see `run`).
        let searcher = self.lexical.searcher();
        if searcher.num_docs() as usize != snap.items.len()
            && st.manifest.lexical_tokenizer == Some(self.tokenizer)
        {
            self.rebuild_lexical(st, &snap.items)?;
        }
        let present: HashSet<u64> = snap.items.keys().copied().collect();
        st.manifest.files.retain(|path, e| {
            !drop_paths.contains(path) && e.chunk_ids.iter().all(|id| present.contains(id))
        });
        self.save_manifest(&mut st.manifest, st.next_id)?;
        vectors.save(&self.dir.join(VECTORS_FILE))?;
        doc_vectors.save(&self.dir.join(DOC_VECTORS_FILE))?;
        self.publish_from(
            &snap,
            snap.items.clone(),
            Arc::new(vectors),
            Arc::new(doc_vectors),
            self.lexical.searcher(),
            snap.graph.clone(),
            st.manifest.files.len(),
            snap.updated_at,
            manifest_stale(&st.manifest),
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
        // The first pass since this index was opened checks stored state against the tree.
        let first_pass = !self.repaired.load(Ordering::Relaxed);
        // Different model (or forced): every file is chunked and embedded again, from the whole
        // tree, a batch at a time. Each batch replaces its files' items, so the index stays
        // searchable throughout and a file that cannot be read keeps the items it had.
        let new_model = st.manifest.model_id != model_id;
        let rebuild = new_model || opts.force || st.manifest.rebuilding;
        // Paths a failed batch may have left half-written: indexed again whatever their stat or
        // hash says, until a pass succeeds.
        let dirty: HashSet<String> = st.manifest.retry_paths.iter().cloned().collect();
        // A failed pass's scope joins this one's: the whole tree, or its paths too.
        let scope: Option<Vec<String>> = match opts.paths {
            Some(mut paths) if !rebuild && !st.manifest.retry_full => {
                paths.extend(st.manifest.retry_paths.iter().cloned());
                paths.sort();
                paths.dedup();
                Some(paths)
            }
            _ => None,
        };
        let paths = scope.as_deref();
        let registry = ctx.chunker.registry().clone();
        let accepts = |p: &Path| registry.accepts(p);
        let mut walked = match paths {
            None => walk_tree(&self.root, &accepts),
            // Watcher events: only files a full walk would index.
            Some(paths) => walk_paths(&self.root, &accepts, paths),
        };
        if !walked.unread.is_empty() {
            tracing::warn!(
                "{}: could not read {}; their indexed files are kept",
                self.root.display(),
                walked.unread.join(", ")
            );
        }
        if rebuild {
            st.manifest.chunker.clear();
            st.manifest.extensions.clear();
            st.manifest.rebuilding = true;
        }
        if new_model {
            // The old vectors cannot be searched with this model; items stay for lexical search
            // until their files are embedded again. The empty checkpoints are written before the
            // manifest names the new model, so a crash cannot pair the old vectors with it.
            let vectors = VectorIndex::new(self.dim, self.precision)?;
            let doc_vectors = VectorIndex::new(self.dim, self.precision)?;
            vectors.save(&self.dir.join(VECTORS_FILE))?;
            doc_vectors.save(&self.dir.join(DOC_VECTORS_FILE))?;
            st.manifest.model_id = model_id.clone();
            let snap = self.snapshot();
            self.publish_from(
                &snap,
                snap.items.clone(),
                Arc::new(vectors),
                Arc::new(doc_vectors),
                snap.searcher.clone(),
                snap.graph.clone(),
                snap.files_indexed,
                snap.updated_at,
                manifest_stale(&st.manifest),
            );
            self.repaired.store(true, Ordering::Relaxed);
        } else if rebuild {
            self.publish_stale(manifest_stale(&st.manifest));
        }
        if rebuild {
            // Recorded before any batch, so a pass that fails part-way is resumed.
            self.save_manifest(&mut st.manifest, st.next_id)?;
        }
        if !self.repaired.load(Ordering::Relaxed) {
            self.repair(st, ctx)?;
        }
        if st.manifest.lexical_tokenizer != Some(self.tokenizer) {
            // Another tokenizer's index was kept in step meanwhile, or none recorded which:
            // this one may hold entries of items long gone, under ids since given to others.
            let snap = self.snapshot();
            self.rebuild_lexical(st, &snap.items)?;
            st.manifest.lexical_tokenizer = Some(self.tokenizer);
            self.save_manifest(&mut st.manifest, st.next_id)?;
        }
        ctx.cache.register_model(&model_id, self.dim, None)?;
        // Re-chunk unchanged files whose chunking changed since the last full pass: every file
        // for a settings change, else those of extensions whose language, queries or mapping
        // changed. Embeddings stay cached.
        let chunker_fp = ctx.chunker.fingerprint();
        let extension_fps = ctx.chunker.registry().extension_fingerprints();
        let full = paths.is_none();
        let rechunk_all = full && st.manifest.chunker != chunker_fp;
        let rechunk_extensions: HashSet<String> = extension_fps
            .iter()
            .filter(|(ext, fp)| full && st.manifest.extensions.get(*ext) != Some(fp))
            .map(|(ext, _)| ext.clone())
            .collect();
        let rechunk = |rel: &str| {
            rechunk_all
                || extension_of(Path::new(rel)).is_some_and(|e| rechunk_extensions.contains(&e))
                || dirty.contains(rel)
        };
        let graph = Arc::new(ProjectGraph::build(&self.root));
        self.reassign_units(st, &graph, first_pass)?;
        // After the units it records are stored, so a crash between the two is seen as a change.
        fs::write(self.dir.join("graph.json"), serde_json::to_vec(&*graph)?)?;

        // Decide which files changed.
        if full {
            st.manifest.skipped_extensions = mem::take(&mut walked.skipped);
            // Files the walk missed keep their items; those due for re-chunking get it once read.
            for (rel, e) in st.manifest.files.iter_mut() {
                if walked.missed(rel) && rechunk(rel) {
                    e.file_hash.clear();
                    e.mtime = 0;
                }
            }
        }
        let candidates = mem::take(&mut walked.files);
        let candidate_set: HashSet<&str> = candidates.iter().map(String::as_str).collect();
        // Indexed paths a successful walk no longer lists; paths never indexed need no removal.
        let mut removed: Vec<String> = {
            let snap = self.snapshot();
            let mut known: HashSet<String> = st.manifest.files.keys().cloned().collect();
            known.extend(snap.items.values().map(|r| r.item.rel_path.clone()));
            // So do paths a failed batch may have written to either store: stored but never
            // published, or only in the lexical index, which only a delete by path reaches.
            known.extend(st.items.stored_paths(paths)?);
            known.extend(dirty.iter().cloned());
            let gone = |p: &String| !candidate_set.contains(p.as_str()) && !walked.missed(p);
            match paths {
                None => known.into_iter().filter(gone).collect(),
                Some(paths) => paths
                    .iter()
                    .filter(|p| known.contains(*p) && gone(p))
                    .cloned()
                    .collect(),
            }
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
                    if !rechunk(&rel)
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
        let mut read_failed: Vec<String> = Vec::new();

        if !removed.is_empty() {
            self.commit_or_record(st, &removed, Vec::new(), &[], &[], &graph)?;
            for p in &removed {
                st.manifest.files.remove(p);
            }
            self.save_manifest(&mut st.manifest, st.next_id)?;
        }

        for batch in changed.chunks(FILES_PER_BATCH) {
            // Read + hash; files whose content is unchanged only get their stat refreshed.
            let (read, failed): (Vec<ReadFile>, Vec<String>) =
                batch.par_iter().partition_map(|(rel, size, mtime)| {
                    match fs::read(self.root.join(rel)) {
                        Ok(bytes) => {
                            let hash = blake3::hash(&bytes).to_hex().to_string();
                            Either::Left((rel.clone(), *size, *mtime, hash, bytes))
                        }
                        Err(_) => Either::Right(rel.clone()),
                    }
                });
            // An unreadable file keeps its items, and its due re-chunking, until it is read.
            for rel in failed {
                if let Some(e) = st.manifest.files.get_mut(&rel)
                    && rechunk(&rel)
                {
                    e.file_hash.clear();
                    e.mtime = 0;
                }
                read_failed.push(rel);
            }
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
                        if !rechunk(&rel)
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
            self.commit_or_record(st, &paths, rows.clone(), &vectors, &doc_vectors, &graph)?;
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
            self.save_manifest(&mut st.manifest, st.next_id)?;
            let elapsed = started.elapsed().as_secs_f64();
            self.set_progress(|p| {
                p.files_done += batch.len();
                let left = p.files_total.saturating_sub(p.files_done);
                p.eta_secs = (p.files_done > 0)
                    .then(|| (elapsed / p.files_done as f64 * left as f64) as u64);
            });
        }
        let snap = self.snapshot();
        // The checkpoints go first: a rebuild is finished only once its vectors are on disk.
        if report.files_changed > 0 || report.files_removed > 0 || !snap.stale.is_empty() {
            snap.vectors.save(&self.dir.join(VECTORS_FILE))?;
            snap.doc_vectors.save(&self.dir.join(DOC_VECTORS_FILE))?;
        }
        if full {
            st.manifest.chunker = chunker_fp;
            st.manifest.extensions = extension_fps;
            st.manifest.rebuilding = false;
        }
        // This pass covered what a failed one left (see `scope`), except dirty paths it could not
        // read, whose stores may still disagree: they stay recorded until a pass reads them.
        st.manifest.retry_full = false;
        st.manifest
            .retry_paths
            .retain(|p| walked.missed(p) || read_failed.contains(p));
        // A full pass finds every unread path again; a partial one settles those it was asked
        // about (read, removed or gone), keeping those it still could not read.
        let mut unread = if full {
            mem::take(&mut walked.unread)
        } else {
            let mut kept = mem::take(&mut st.manifest.unread);
            let requested = paths.unwrap_or_default();
            kept.retain(|u| !requested.contains(u) || read_failed.contains(u));
            kept.extend(walked.unread.iter().cloned());
            kept
        };
        unread.extend(read_failed);
        unread.sort();
        unread.dedup();
        st.manifest.unread = unread;
        st.manifest.updated_at = now_secs();
        self.save_manifest(&mut st.manifest, st.next_id)?;
        // On the first pass too, so a lowered budget applies without new embeddings.
        if report.chunks_embedded > 0 || first_pass {
            let budget = (ctx.config.cache_max_gb * 1e9) as u64;
            match ctx.cache.prune(budget) {
                Ok(0) => {}
                Ok(n) => tracing::info!("pruned {n} cached embeddings over cache_max_gb"),
                Err(e) => tracing::warn!("pruning the embedding cache: {e:#}"),
            }
        }
        self.publish_from(
            &snap,
            snap.items.clone(),
            snap.vectors.clone(),
            snap.doc_vectors.clone(),
            snap.searcher.clone(),
            graph,
            st.manifest.files.len(),
            st.manifest.updated_at,
            manifest_stale(&st.manifest),
        );
        report.secs = started.elapsed().as_secs_f64();
        Ok(report)
    }

    /// Updates stored items' units to `graph` when it differs from the published one (a package
    /// renamed, added or moved), since ownership then changes without any source file changing;
    /// on the `first_pass` regardless, since the graph loaded with the index may not be the one
    /// its units came from.
    fn reassign_units(
        &self,
        st: &mut WriterState,
        graph: &Arc<ProjectGraph>,
        first_pass: bool,
    ) -> anyhow::Result<()> {
        let snap = self.snapshot();
        if !first_pass && *snap.graph == **graph {
            return Ok(());
        }
        let changed: Vec<(u64, Option<String>)> = snap
            .items
            .values()
            .filter_map(|r| {
                let unit = graph.unit_of(&r.item.rel_path).map(String::from);
                (unit != r.unit).then_some((r.id, unit))
            })
            .collect();
        if changed.is_empty() {
            return Ok(());
        }
        st.items.set_units(&changed)?;
        let mut items = (*snap.items).clone();
        for (id, unit) in changed {
            if let Some(row) = items.get_mut(&id) {
                Arc::make_mut(row).unit = unit;
            }
        }
        self.publish_from(
            &snap,
            Arc::new(items),
            snap.vectors.clone(),
            snap.doc_vectors.clone(),
            snap.searcher.clone(),
            graph.clone(),
            snap.files_indexed,
            snap.updated_at,
            snap.stale.clone(),
        );
        Ok(())
    }

    /// Whether `v` can go into this index: of its dimension, and finite.
    fn usable(&self, v: &[f32]) -> bool {
        v.len() == self.dim && v.iter().all(|x| x.is_finite())
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
                Some(v) if self.usable(v) => add.push((r.id, v.as_slice())),
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
        // An unusable cached vector (cached before vectors were checked) is a miss, replaced in
        // the cache once re-embedded.
        cached.retain(|_, v| self.usable(v));
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
            // Checked before caching: a bad vector in the shared cache outlives the bad response.
            anyhow::ensure!(
                vecs.len() == texts.len(),
                "embedder returned {} vectors for {} texts",
                vecs.len(),
                texts.len()
            );
            if let Some(v) = vecs.iter().find(|v| !self.usable(v)) {
                anyhow::bail!(
                    "embedder returned an invalid vector ({} values, expected {} finite ones)",
                    v.len(),
                    self.dim
                );
            }
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

    /// [`Self::commit_batch`], recording `paths` for the next pass to index again (see `dirty` in
    /// `run`) if it fails, when the stores may hold part of it: in the manifest on an error, and
    /// in a journal written first, for a crash between the stores' commits.
    fn commit_or_record(
        &self,
        st: &mut WriterState,
        paths: &[String],
        rows: Vec<ItemRow>,
        vectors: &[Vec<f32>],
        doc_vectors: &[(u64, Vec<f32>)],
        graph: &Arc<ProjectGraph>,
    ) -> anyhow::Result<()> {
        let journal = self.dir.join(BATCH_JOURNAL);
        fs::write(&journal, serde_json::to_vec(paths)?)?;
        self.commit_batch(st, paths, rows, vectors, doc_vectors, graph)
            .inspect_err(|_| st.manifest.retry_paths.extend(paths.iter().cloned()))?;
        match fs::remove_file(&journal) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Adds the paths of a batch interrupted by a crash (see [`Self::commit_or_record`]) to
    /// `manifest`'s retry paths, saved before the journal goes.
    fn recover_batch(&self, manifest: &mut Manifest, next_id: u64) -> anyhow::Result<()> {
        let journal = self.dir.join(BATCH_JOURNAL);
        let paths: Vec<String> = match fs::read(&journal) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        manifest.retry_paths.extend(paths);
        manifest.retry_paths.sort();
        manifest.retry_paths.dedup();
        self.save_manifest(manifest, next_id)?;
        fs::remove_file(&journal)?;
        Ok(())
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
        // The items stored and the items published for `paths` differ after a batch that failed
        // between the two (stored, never published): both go.
        let path_set: HashSet<&str> = paths.iter().map(String::as_str).collect();
        let mut old_ids = st.items.ids_for_paths(paths)?;
        old_ids.extend(
            snap.items
                .values()
                .filter(|r| path_set.contains(r.item.rel_path.as_str()))
                .map(|r| r.id),
        );
        old_ids.sort_unstable();
        old_ids.dedup();
        // One write for both: the items commit only once the lexical entries have, and a failed
        // lexical write is rolled back, so neither store gets ahead of the other.
        let WriterState { items, writer, .. } = st;
        items.replace_files_with(paths, &rows, || {
            self.lexical.delete_paths(writer, paths);
            let written = rows
                .iter()
                .try_for_each(|r| self.lexical.add(writer, r))
                .and_then(|()| Ok(writer.commit().map(|_| ())?));
            if written.is_err()
                && let Err(e) = writer.rollback()
            {
                tracing::warn!("rolling back the lexical index: {e:#}");
            }
            written
        })?;
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
        let walked = walk_tree(&self.root, accepts);
        let files = &walked.files;
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
        changed
            + m.files
                .keys()
                .filter(|k| !set.contains(k.as_str()) && !walked.missed(k))
                .count()
    }
}

/// Why an index with this manifest may be out of date: an unfinished rebuild (see
/// [`Manifest::rebuilding`]), a failed pass's work (see [`Manifest::retry_full`]) or paths it
/// could not read. Every snapshot a writer publishes takes its reasons from here, so none claims
/// freshness the manifest does not.
fn manifest_stale(m: &Manifest) -> Vec<String> {
    let rebuilding = m
        .rebuilding
        .then(|| "a rebuild of the index has not finished; the next index resumes it".to_string());
    let retry = (m.retry_full || !m.retry_paths.is_empty()).then(|| RETRY_REASON.to_string());
    rebuilding
        .into_iter()
        .chain(retry)
        .chain(unread_reason(&m.unread))
        .collect()
}

/// Why an index with `unread` paths (see [`Manifest::unread`]) may be out of date.
fn unread_reason(unread: &[String]) -> Option<String> {
    let paths: Vec<&str> = unread
        .iter()
        .map(|u| if u.is_empty() { "." } else { u.as_str() })
        .collect();
    (!paths.is_empty()).then(|| {
        format!(
            "could not read {}; files there may be out of date",
            paths.join(", ")
        )
    })
}

/// How many keys of `index` belong to no item in `ids`.
fn stray_count(index: &VectorIndex, ids: &HashSet<u64>) -> usize {
    let kept = ids.iter().filter(|id| index.contains(**id)).count();
    index.len().saturating_sub(kept)
}
