//! Helpers shared by unit and integration tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::Backend;
use crate::embed::{Embedder, InputKind};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty directory under the system temp dir.
pub fn tempdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!(
        "lodestone-test-{}-{n}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&d).expect("create temp dir");
    d
}

/// Writes each `(relative path, contents)` under `root`, creating directories.
pub fn write_tree(root: &Path, files: &[(&str, &str)]) {
    for (rel, text) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().expect("file has a parent")).expect("create dirs");
        fs::write(path, text).expect("write file");
    }
}

/// An embedder that fails documents (not queries) while `flag` exists: an embedding service down
/// for indexing.
pub struct FailWhile {
    pub inner: Arc<dyn Embedder>,
    pub flag: PathBuf,
}

impl Embedder for FailWhile {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn backend(&self) -> Backend {
        self.inner.backend()
    }
    fn embed(&self, texts: &[&str], kind: InputKind) -> anyhow::Result<Vec<Vec<f32>>> {
        if kind == InputKind::Document && self.flag.exists() {
            anyhow::bail!("embedding service unavailable");
        }
        self.inner.embed(texts, kind)
    }
}
