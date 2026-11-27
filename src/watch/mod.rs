//! Freshness: one recursive watcher per worktree (500 ms debounce, coalesced per path) feeding a
//! per-worktree indexing task. Git HEAD/ref changes trigger a full rescan. If the watcher fails
//! the worktree degrades to a 60 s rescan poll, reported in `status`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

pub use crate::checkout::git_dir;
use crate::index::{IndexOptions, WorktreeIndex};
use crate::registry::Engine;

const DEBOUNCE: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_secs(60);
/// How long a failed indexing pass waits for the next change before it is retried.
const RETRY: Duration = Duration::from_secs(60);

/// How the watcher state `status` reports starts after the watcher failed: errors it recovers
/// from with a rescan, or no watcher at all.
const DEGRADED: &str = "degraded: ";
const POLLING: &str = "polling every 60s ";

/// Whether watcher state `state`, as `status` reports it, says the watcher failed.
pub fn failed(state: &str) -> bool {
    state.starts_with(DEGRADED) || state.starts_with(POLLING)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Paths(BTreeSet<String>),
    Rescan,
}

struct Handle {
    _debouncer:
        Option<Debouncer<notify_debouncer_full::notify::RecommendedWatcher, RecommendedCache>>,
    tx: mpsc::UnboundedSender<Change>,
    /// The fallback rescan timer, when the watcher failed.
    poll: Option<tokio::task::JoinHandle<()>>,
    /// The index this handle's worker writes.
    index: Arc<WorktreeIndex>,
}

impl Handle {
    /// Whether the index is still in use, not retired (see [`WorktreeIndex::retire`]).
    fn live(&self) -> bool {
        !self.index.is_retired()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // The timer holds a sender, which would keep the worker alive after `unwatch`.
        if let Some(p) = &self.poll {
            p.abort();
        }
    }
}

#[derive(Default)]
pub struct Watchers {
    handles: Arc<Mutex<HashMap<PathBuf, Handle>>>,
    /// Indexing passes running now, including those of worktrees no longer watched.
    busy: Arc<AtomicUsize>,
}

/// Ignore files the index walk reads at any level; a change to one can show or hide any file.
const IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".lodestoneignore"];

/// Classifies raw event paths for a worktree.
pub struct EventFilter {
    root: PathBuf,
    /// Refreshed when the root becomes, or stops being, a git checkout.
    git_dir: RwLock<Option<PathBuf>>,
    /// Reloaded when an ignore file changes.
    ignore: RwLock<Gitignore>,
}

impl EventFilter {
    pub fn new(root: &Path) -> Self {
        let git = git_dir(root);
        Self {
            ignore: RwLock::new(Self::matcher(root, git.as_deref())),
            root: root.to_path_buf(),
            git_dir: RwLock::new(git),
        }
    }

    /// The root's ignore rules, which drop events before they reach the index walk. Added from
    /// lowest precedence to highest, as the walk ranks them, since the last matching rule wins.
    fn matcher(root: &Path, git_dir: Option<&Path>) -> Gitignore {
        let mut b = GitignoreBuilder::new(root);
        for f in git_dir
            .map(|g| g.join("info/exclude"))
            .into_iter()
            .chain([".gitignore", ".ignore", ".lodestoneignore"].map(|f| root.join(f)))
        {
            if f.exists() {
                let _ = b.add(f);
            }
        }
        b.build().unwrap_or_else(|_| Gitignore::empty())
    }

    /// The worktree's git directory, as last seen.
    pub fn git_dir(&self) -> Option<PathBuf> {
        self.git_dir
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn is_git_ref(rel: &Path) -> bool {
        rel == Path::new("HEAD") || rel.starts_with("refs") || rel == Path::new("packed-refs")
    }

    /// Folds event paths into a change set; `None` if nothing relevant happened. Asks for a full
    /// rescan when what the index covers may have changed: a git ref moved, an ignore file
    /// changed (reloading the rules), or a checkout appeared or went away inside the tree.
    pub fn classify<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> Option<Change> {
        let mut out = BTreeSet::new();
        let (mut rescan, mut reload) = (false, false);
        let mut git_dir = self
            .git_dir
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // A pointer to a git directory not created yet resolves once it is.
        if git_dir
            .as_deref()
            .is_some_and(|g| g.canonicalize().ok().as_deref() != Some(g))
        {
            git_dir = crate::checkout::git_dir(&self.root);
            *self.git_dir.write().unwrap_or_else(|e| e.into_inner()) = git_dir.clone();
        }
        for p in paths {
            // The root becoming a checkout, or ceasing to be one, changes which nested checkouts
            // its index owns.
            if p == self.root.join(".git") {
                git_dir = crate::checkout::git_dir(&self.root);
                *self.git_dir.write().unwrap_or_else(|e| e.into_inner()) = git_dir.clone();
                (rescan, reload) = (true, true);
                continue;
            }
            if let Some(g) = &git_dir
                && let Ok(rel) = p.strip_prefix(g)
            {
                rescan |= Self::is_git_ref(rel);
                reload |= rel == Path::new("info/exclude");
                continue;
            }
            let Ok(rel) = p.strip_prefix(&self.root) else {
                continue;
            };
            if rel.starts_with(".git") {
                continue;
            }
            // Ignore files and checkouts count wherever the walk enters their directory, even
            // when the rules ignore the marker itself (`.git` listed in `.gitignore`).
            let name = rel.file_name();
            let in_tree = || {
                rel.parent()
                    .is_none_or(|d| d.as_os_str().is_empty() || !self.ignored(d, true))
            };
            if name.is_some_and(|n| IGNORE_FILES.iter().any(|f| n == *f)) && in_tree() {
                reload = true;
                continue;
            }
            if name.is_some_and(|n| n == ".git") && in_tree() {
                rescan = true;
                continue;
            }
            if self.ignored(rel, p.is_dir()) {
                continue;
            }
            // Cache directories (`target/` and friends carry CACHEDIR.TAG).
            if rel.ancestors().skip(1).any(|a| {
                !a.as_os_str().is_empty() && self.root.join(a).join("CACHEDIR.TAG").exists()
            }) {
                continue;
            }
            if p.is_dir() {
                continue;
            }
            out.insert(rel.to_string_lossy().replace('\\', "/"));
        }
        if reload {
            *self.ignore.write().unwrap_or_else(|e| e.into_inner()) =
                Self::matcher(&self.root, git_dir.as_deref());
        }
        if rescan || reload {
            Some(Change::Rescan)
        } else {
            (!out.is_empty()).then_some(Change::Paths(out))
        }
    }

    /// Whether the root's rules ignore `rel` and no ignore file below the root, which could
    /// re-include it, is in a directory above it. Anything else is left to the index walk.
    fn ignored(&self, rel: &Path, is_dir: bool) -> bool {
        self.ignore
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
            && !rel.ancestors().skip(1).any(|a| {
                !a.as_os_str().is_empty()
                    && IGNORE_FILES
                        .iter()
                        .any(|f| self.root.join(a).join(f).exists())
            })
    }
}

impl Watchers {
    fn set_state(engine: &Engine, root: &Path, state: String) {
        if let Ok(mut m) = engine.watcher_state.write() {
            m.insert(root.to_path_buf(), state);
        }
    }

    /// Starts watching `w` (idempotent) and its indexing task, replacing the watch of a retired
    /// index of the same root.
    pub fn watch(&self, engine: &Arc<Engine>, w: Arc<WorktreeIndex>) {
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        if handles.get(&w.root).is_some_and(Handle::live) {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel::<Change>();
        let filter = Arc::new(EventFilter::new(&w.root));
        let events_tx = tx.clone();
        let err_engine = engine.clone();
        let err_root = w.root.clone();
        let root = w.root.clone();
        // A git directory outside the tree is watched apart: the one there at the start below,
        // one that appears later (`git init --separate-git-dir`) by `external`.
        let outside = move |g: Option<PathBuf>| g.filter(|g| !g.starts_with(&root));
        let mut external = ExternalGit {
            path: outside(filter.git_dir()).filter(|g| g.exists()),
            watcher: None,
        };
        let handler_filter = filter.clone();
        let debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
            Ok(events) => {
                let filter = &handler_filter;
                if let Some(c) = filter.classify(
                    events
                        .iter()
                        .flat_map(|e| e.paths.iter().map(PathBuf::as_path)),
                ) {
                    let _ = events_tx.send(c);
                }
                external.follow(outside(filter.git_dir()), filter, &events_tx);
            }
            Err(errors) => {
                let msg = errors
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                tracing::warn!("watcher error for {}: {msg}", err_root.display());
                Self::set_state(
                    &err_engine,
                    &err_root,
                    format!("{DEGRADED}{msg}; full rescan queued"),
                );
                let _ = events_tx.send(Change::Rescan);
            }
        })
        .and_then(|mut d| {
            d.watch(&w.root, RecursiveMode::Recursive)?;
            if let Some(g) = git_dir(&w.root)
                && !g.starts_with(&w.root)
                && g.exists()
            {
                d.watch(&g, RecursiveMode::Recursive)?;
            }
            Ok(d)
        });
        let mut poll = None;
        let debouncer = match debouncer {
            Ok(d) => {
                Self::set_state(
                    engine,
                    &w.root,
                    "watching (fsevents/inotify, 500ms debounce)".into(),
                );
                Some(d)
            }
            Err(e) => {
                tracing::warn!(
                    "watcher failed for {}: {e}; polling every 60s",
                    w.root.display()
                );
                Self::set_state(engine, &w.root, format!("{POLLING}(watcher failed: {e})"));
                let poll_tx = tx.clone();
                poll = Some(tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(POLL).await;
                        if poll_tx.send(Change::Rescan).is_err() {
                            break;
                        }
                    }
                }));
                None
            }
        };
        tokio::spawn(worker(
            engine.clone(),
            w.clone(),
            rx,
            self.busy.clone(),
            Arc::downgrade(&self.handles),
        ));
        handles.insert(
            w.root.clone(),
            Handle {
                _debouncer: debouncer,
                tx,
                poll,
                index: w.clone(),
            },
        );
    }

    /// Stops watching the indexes retired since they were watched, and returns their roots; a
    /// watch started since for a new index of the same root is kept.
    pub fn unwatch_retired(&self) -> Vec<PathBuf> {
        let mut dropped = Vec::new();
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|root, h| {
                h.live() || {
                    dropped.push(root.clone());
                    false
                }
            });
        dropped
    }

    /// Stops watching `root`. Its worker finishes any queued pass, then exits.
    pub fn unwatch(&self, engine: &Engine, root: &Path) {
        let removed = self
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(root);
        if removed.is_some() {
            Self::set_state(engine, root, "not watched (no active session)".into());
        }
    }

    pub fn is_watched(&self, root: &Path) -> bool {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(root)
            .is_some_and(Handle::live)
    }

    /// Whether an indexing pass is running.
    pub fn indexing(&self) -> bool {
        self.busy.load(Ordering::SeqCst) > 0
    }

    pub fn watched(&self) -> Vec<PathBuf> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Queues an indexing pass for `w` on its worker (starting the watcher if needed).
    pub fn spawn_index(&self, engine: &Arc<Engine>, w: Arc<WorktreeIndex>, opts: IndexOptions) {
        self.watch(engine, w.clone());
        let handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = handles.get(&w.root) {
            let change = match opts.paths {
                Some(p) => Change::Paths(p.into_iter().collect()),
                None => Change::Rescan,
            };
            // Before queueing, so a search right after reports the work as pending; the worker
            // clears it when done.
            if change == Change::Rescan {
                w.set_pending(Some("full rescan".into()));
            }
            let _ = h.tx.send(change);
        }
    }
}

type Watcher = Debouncer<notify_debouncer_full::notify::RecommendedWatcher, RecommendedCache>;

/// The git directory outside a worktree being watched (`path`), and the watcher started for it
/// after the worktree's (`None` for the one there from the start, which the worktree's watches).
struct ExternalGit {
    path: Option<PathBuf>,
    watcher: Option<Watcher>,
}

impl ExternalGit {
    /// Watches `git` (the worktree's git directory, when outside it) instead, once watching it
    /// works: until then (it may not exist yet) the current watch stays, and the next call
    /// tries again.
    fn follow(
        &mut self,
        git: Option<PathBuf>,
        filter: &Arc<EventFilter>,
        tx: &mpsc::UnboundedSender<Change>,
    ) {
        if git == self.path {
            return;
        }
        match git.as_deref() {
            None => self.watcher = None,
            Some(g) => match git_watcher(filter, g, tx) {
                Some(w) => self.watcher = Some(w),
                None => return,
            },
        }
        self.path = git;
    }
}

/// Watches `git_dir`, outside the worktree, for ref changes, which `filter` turns into rescans.
fn git_watcher(
    filter: &Arc<EventFilter>,
    git_dir: &Path,
    tx: &mpsc::UnboundedSender<Change>,
) -> Option<Watcher> {
    let (filter, tx) = (filter.clone(), tx.clone());
    let mut d = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| {
        if let Ok(events) = res
            && let Some(c) = filter.classify(
                events
                    .iter()
                    .flat_map(|e| e.paths.iter().map(PathBuf::as_path)),
            )
        {
            let _ = tx.send(c);
        }
    })
    .ok()?;
    d.watch(git_dir, RecursiveMode::Recursive).ok()?;
    Some(d)
}

/// Serializes indexing for one worktree, coalescing everything queued while a pass runs. What a
/// failed pass was to index stays pending, and is retried with the next change or after
/// [`RETRY`], until a pass succeeds.
async fn worker(
    engine: Arc<Engine>,
    w: Arc<WorktreeIndex>,
    mut rx: mpsc::UnboundedReceiver<Change>,
    busy: Arc<AtomicUsize>,
    handles: Weak<Mutex<HashMap<PathBuf, Handle>>>,
) {
    // (rescan, paths) a failed pass left.
    let mut left: Option<(bool, BTreeSet<String>)> = None;
    loop {
        let first = if left.is_none() {
            match rx.recv().await {
                Some(c) => Some(c),
                None => break,
            }
        } else {
            match tokio::time::timeout(RETRY, rx.recv()).await {
                Ok(Some(c)) => Some(c),
                Ok(None) => break,
                Err(_) => None,
            }
        };
        let (mut rescan, mut paths) = left.take().unwrap_or_default();
        for change in first
            .into_iter()
            .chain(std::iter::from_fn(|| rx.try_recv().ok()))
        {
            match change {
                Change::Rescan => rescan = true,
                Change::Paths(p) => paths.extend(p),
            }
        }
        let what = if rescan {
            "full rescan".to_string()
        } else {
            format!("{} changed files", paths.len())
        };
        w.set_pending(Some(what.clone()));
        let opts = IndexOptions {
            force: false,
            paths: (!rescan).then(|| paths.iter().cloned().collect()),
        };
        busy.fetch_add(1, Ordering::SeqCst);
        let result = match engine.index_context() {
            Ok(ctx) => w.index(Arc::new(ctx), opts).await,
            Err(e) => Err(e),
        };
        busy.fetch_sub(1, Ordering::SeqCst);
        match result {
            Ok(r) => tracing::info!(
                "{}: indexed {} files ({} embedded) in {:.2}s",
                w.root.display(),
                r.files_changed,
                r.chunks_embedded,
                r.secs
            ),
            Err(e) => {
                tracing::warn!("{}: indexing failed: {e:#}", w.root.display());
                left = Some((rescan, paths));
            }
        }
        if w.is_retired() {
            // Unregistered, here or by another process: stops watching it, unless a new index of
            // the same worktree is watched already.
            let Some(handles) = handles.upgrade() else {
                break;
            };
            let mut handles = handles.lock().unwrap_or_else(|e| e.into_inner());
            if handles
                .get(&w.root)
                .is_some_and(|h| Arc::ptr_eq(&h.index, &w))
            {
                handles.remove(&w.root);
                Watchers::set_state(&engine, &w.root, "not watched (unregistered)".into());
            }
            break;
        }
        if rx.is_empty() {
            w.set_pending(
                left.is_some()
                    .then(|| format!("{what}, retried after a failed pass")),
            );
            w.release_writer();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_events() {
        let root = crate::testutil::tempdir();
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(
            root.join("target/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();
        let f = EventFilter::new(&root);
        let p = |s: &str| root.join(s);
        assert_eq!(f.classify([p(".git/HEAD").as_path()]), Some(Change::Rescan));
        assert_eq!(
            f.classify([p(".git/refs/heads/main").as_path()]),
            Some(Change::Rescan)
        );
        assert_eq!(f.classify([p(".git/objects/ab/cd").as_path()]), None);
        assert_eq!(f.classify([p("target/debug/x.rs").as_path()]), None);
        assert_eq!(f.classify([p("gen/out.rs").as_path()]), None);
        assert_eq!(
            f.classify([p("src/a.rs").as_path(), p("src/a.rs").as_path()]),
            Some(Change::Paths(BTreeSet::from(["src/a.rs".to_string()])))
        );
    }

    #[test]
    fn an_ignore_file_change_reloads_the_rules_and_rescans() {
        let root = crate::testutil::tempdir();
        std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();
        let f = EventFilter::new(&root);
        let gen_file = root.join("gen/out.rs");
        assert_eq!(f.classify([gen_file.as_path()]), None);
        std::fs::write(root.join(".gitignore"), "").unwrap();
        assert_eq!(
            f.classify([root.join(".gitignore").as_path()]),
            Some(Change::Rescan)
        );
        assert_eq!(
            f.classify([gen_file.as_path()]),
            Some(Change::Paths(BTreeSet::from(["gen/out.rs".to_string()])))
        );
        assert_eq!(
            f.classify([root.join("src/.lodestoneignore").as_path()]),
            Some(Change::Rescan)
        );
        // In one batch with a ref change, the ignore file still reloads the rules.
        std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        let f = EventFilter::new(&root);
        std::fs::write(root.join(".gitignore"), "").unwrap();
        let batch = [root.join(".git/HEAD"), root.join(".gitignore")];
        assert_eq!(
            f.classify(batch.iter().map(PathBuf::as_path)),
            Some(Change::Rescan)
        );
        assert!(f.classify([gen_file.as_path()]).is_some());
    }

    #[test]
    fn nested_rules_and_checkouts_reach_the_index_walk() {
        let root = crate::testutil::tempdir();
        std::fs::write(root.join(".gitignore"), "*.gen.rs\n").unwrap();
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::write(root.join("child/.gitignore"), "!keep.gen.rs\n").unwrap();
        let f = EventFilter::new(&root);
        // The walk decides: a nested rule may re-include what the root's rules ignore.
        assert_eq!(
            f.classify([root.join("child/keep.gen.rs").as_path()]),
            Some(Change::Paths(BTreeSet::from([
                "child/keep.gen.rs".to_string()
            ])))
        );
        assert_eq!(f.classify([root.join("other.gen.rs").as_path()]), None);
        // `git init` in a subdirectory moves its files to another index, even with `.git`
        // ignored; not inside an ignored directory, which the walk never enters.
        std::fs::write(root.join(".gitignore"), "*.gen.rs\n.git\ngen/\n").unwrap();
        let f = EventFilter::new(&root);
        std::fs::create_dir_all(root.join("child/.git")).unwrap();
        assert_eq!(
            f.classify([root.join("child/.git").as_path()]),
            Some(Change::Rescan)
        );
        assert_eq!(f.classify([root.join("gen/x/.git").as_path()]), None);
        assert_eq!(f.classify([root.join("gen/.gitignore").as_path()]), None);
    }

    #[test]
    fn the_root_becoming_a_checkout_rescans() {
        let root = crate::testutil::tempdir();
        let f = EventFilter::new(&root);
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        let marker = root.join(".git");
        assert_eq!(f.classify([marker.as_path()]), Some(Change::Rescan));
        // Its refs are watched from then on.
        let head = root.join(".git/refs/heads/main");
        assert_eq!(f.classify([head.as_path()]), Some(Change::Rescan));
        assert_eq!(f.classify([root.join(".git/objects/x").as_path()]), None);
    }

    #[test]
    fn a_git_dir_created_after_its_pointer_is_found() {
        let base = crate::testutil::tempdir().canonicalize().unwrap();
        let root = base.join("wt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "gitdir: ../external.git\n").unwrap();
        let f = EventFilter::new(&root);
        std::fs::create_dir_all(base.join("external.git/refs/heads")).unwrap();
        f.classify([root.join("src/a.rs").as_path()]);
        assert_eq!(f.git_dir(), Some(base.join("external.git")));
        let head = base.join("external.git/HEAD");
        assert_eq!(f.classify([head.as_path()]), Some(Change::Rescan));
    }

    #[test]
    fn a_failed_retarget_keeps_the_watch_it_had() {
        let base = crate::testutil::tempdir().canonicalize().unwrap();
        let (a, b) = (base.join("a.git"), base.join("b.git"));
        std::fs::create_dir_all(&a).unwrap();
        let filter = Arc::new(EventFilter::new(&base.join("wt")));
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut external = ExternalGit {
            path: None,
            watcher: None,
        };
        external.follow(Some(a.clone()), &filter, &tx);
        assert_eq!(external.path.as_ref(), Some(&a));
        // B does not exist: A stays watched, and returning to it keeps that watch.
        external.follow(Some(b.clone()), &filter, &tx);
        assert_eq!(external.path.as_ref(), Some(&a));
        assert!(external.watcher.is_some());
        external.follow(Some(a.clone()), &filter, &tx);
        assert!(external.watcher.is_some());
        // Once B exists, it is watched.
        std::fs::create_dir_all(&b).unwrap();
        external.follow(Some(b.clone()), &filter, &tx);
        assert_eq!(external.path, Some(b));
    }

    #[test]
    fn root_rules_rank_as_the_walk_ranks_them() {
        let root = crate::testutil::tempdir();
        std::fs::write(root.join(".gitignore"), "*.gen.rs\n").unwrap();
        std::fs::write(root.join(".ignore"), "!root.gen.rs\n").unwrap();
        let f = EventFilter::new(&root);
        assert!(f.classify([root.join("root.gen.rs").as_path()]).is_some());
        assert_eq!(f.classify([root.join("other.gen.rs").as_path()]), None);
    }
}
