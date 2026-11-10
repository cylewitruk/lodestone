//! Freshness: one recursive watcher per worktree (500 ms debounce, coalesced per path) feeding a
//! per-worktree indexing task. Git HEAD/ref changes trigger a full rescan. If the watcher fails
//! the worktree degrades to a 60 s rescan poll, reported in `status`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

use crate::index::{IndexOptions, WorktreeIndex};
use crate::registry::Engine;

const DEBOUNCE: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Paths(BTreeSet<String>),
    Rescan,
}

struct Handle {
    _debouncer:
        Option<Debouncer<notify_debouncer_full::notify::RecommendedWatcher, RecommendedCache>>,
    tx: mpsc::UnboundedSender<Change>,
}

#[derive(Default)]
pub struct Watchers {
    handles: Mutex<HashMap<PathBuf, Handle>>,
}

/// The git directory of a worktree: `.git/` or the `gitdir:` target of a `.git` file.
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    let p = Path::new(target);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    })
}

/// Classifies raw event paths for a worktree.
pub struct EventFilter {
    root: PathBuf,
    git_dir: Option<PathBuf>,
    ignore: Gitignore,
}

impl EventFilter {
    pub fn new(root: &Path) -> Self {
        let mut b = GitignoreBuilder::new(root);
        let git = git_dir(root);
        for f in [root.join(".gitignore"), root.join(".lodestoneignore")]
            .into_iter()
            .chain(git.as_ref().map(|g| g.join("info/exclude")))
        {
            if f.exists() {
                let _ = b.add(f);
            }
        }
        Self {
            root: root.to_path_buf(),
            git_dir: git,
            ignore: b.build().unwrap_or_else(|_| Gitignore::empty()),
        }
    }

    fn is_git_ref(rel: &Path) -> bool {
        rel == Path::new("HEAD") || rel.starts_with("refs") || rel == Path::new("packed-refs")
    }

    /// Folds event paths into a change set; `None` if nothing relevant happened.
    pub fn classify<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> Option<Change> {
        let mut out = BTreeSet::new();
        for p in paths {
            if let Some(g) = &self.git_dir
                && let Ok(rel) = p.strip_prefix(g)
            {
                if Self::is_git_ref(rel) {
                    return Some(Change::Rescan);
                }
                continue;
            }
            let Ok(rel) = p.strip_prefix(&self.root) else {
                continue;
            };
            if rel.starts_with(".git") {
                continue;
            }
            if self
                .ignore
                .matched_path_or_any_parents(rel, p.is_dir())
                .is_ignore()
            {
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
        (!out.is_empty()).then_some(Change::Paths(out))
    }
}

impl Watchers {
    fn set_state(engine: &Engine, root: &Path, state: String) {
        if let Ok(mut m) = engine.watcher_state.write() {
            m.insert(root.to_path_buf(), state);
        }
    }

    /// Starts watching `w` (idempotent) and its indexing task.
    pub fn watch(&self, engine: &Arc<Engine>, w: Arc<WorktreeIndex>) {
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        if handles.contains_key(&w.root) {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel::<Change>();
        let filter = EventFilter::new(&w.root);
        let events_tx = tx.clone();
        let err_engine = engine.clone();
        let err_root = w.root.clone();
        let debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
            Ok(events) => {
                if let Some(c) = filter.classify(
                    events
                        .iter()
                        .flat_map(|e| e.paths.iter().map(PathBuf::as_path)),
                ) {
                    let _ = events_tx.send(c);
                }
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
                    format!("degraded: {msg}; full rescan queued"),
                );
                let _ = events_tx.send(Change::Rescan);
            }
        })
        .and_then(|mut d| {
            d.watch(&w.root, RecursiveMode::Recursive)?;
            if let Some(g) = git_dir(&w.root)
                && !g.starts_with(&w.root)
            {
                d.watch(&g, RecursiveMode::Recursive)?;
            }
            Ok(d)
        });
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
                Self::set_state(
                    engine,
                    &w.root,
                    format!("polling every 60s (watcher failed: {e})"),
                );
                let poll_tx = tx.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(POLL).await;
                        if poll_tx.send(Change::Rescan).is_err() {
                            break;
                        }
                    }
                });
                None
            }
        };
        tokio::spawn(worker(engine.clone(), w.clone(), rx));
        handles.insert(
            w.root.clone(),
            Handle {
                _debouncer: debouncer,
                tx,
            },
        );
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
            let _ = h.tx.send(change);
        }
    }
}

/// Serializes indexing for one worktree, coalescing everything queued while a pass runs.
async fn worker(
    engine: Arc<Engine>,
    w: Arc<WorktreeIndex>,
    mut rx: mpsc::UnboundedReceiver<Change>,
) {
    while let Some(first) = rx.recv().await {
        let mut rescan = first == Change::Rescan;
        let mut paths = match first {
            Change::Paths(p) => p,
            Change::Rescan => BTreeSet::new(),
        };
        while let Ok(more) = rx.try_recv() {
            match more {
                Change::Rescan => rescan = true,
                Change::Paths(p) => paths.extend(p),
            }
        }
        let what = if rescan {
            "full rescan".to_string()
        } else {
            format!("{} changed files", paths.len())
        };
        w.set_pending(Some(what));
        let opts = IndexOptions {
            force: false,
            paths: (!rescan).then(|| paths.into_iter().collect()),
        };
        let result = match engine.index_context() {
            Ok(ctx) => w.index(Arc::new(ctx), opts).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(r) => tracing::info!(
                "{}: indexed {} files ({} embedded) in {:.2}s",
                w.root.display(),
                r.files_changed,
                r.chunks_embedded,
                r.secs
            ),
            Err(e) => tracing::warn!("{}: indexing failed: {e:#}", w.root.display()),
        }
        if rx.is_empty() {
            w.set_pending(None);
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
    fn git_file_worktree() {
        let root = crate::testutil::tempdir();
        std::fs::write(root.join(".git"), "gitdir: /repo/.git/worktrees/wt\n").unwrap();
        assert_eq!(
            git_dir(&root),
            Some(PathBuf::from("/repo/.git/worktrees/wt"))
        );
    }
}
