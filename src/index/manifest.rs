//! `manifest.json`: per-file state of a worktree index. Written last in every indexing batch, so
//! a file listed here has its items, lexical entries and vectors (or cache entries) on disk.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub file_hash: String,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime: i64,
    pub size: u64,
    pub chunk_ids: Vec<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub root: PathBuf,
    pub model_id: String,
    pub files: BTreeMap<String, FileEntry>,
    /// Files seen by the last full walk that no grammar or text rule accepts, per extension.
    #[serde(default)]
    pub skipped_extensions: BTreeMap<String, u64>,
    /// Unix seconds of the last completed indexing run.
    #[serde(default)]
    pub updated_at: u64,
}

impl Manifest {
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic write: temp file then rename.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}
