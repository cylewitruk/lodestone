//! `manifest.json`: per-file state of a worktree index. Written last in every indexing batch, so
//! a file listed here has its items, lexical entries and vectors (or cache entries) on disk.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::lexical::TokenizerKind;

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
    /// Chunker settings fingerprint the files were chunked with; a mismatch re-chunks every file.
    #[serde(default)]
    pub chunker: String,
    /// Per-extension chunking fingerprints the files were chunked with; a mismatch re-chunks
    /// that extension's files.
    #[serde(default)]
    pub extensions: BTreeMap<String, String>,
    /// Unix seconds of the last completed indexing run.
    #[serde(default)]
    pub updated_at: u64,
    /// Paths (`""`: the root) the last walk or read could not get at; their files keep the items
    /// they had until a pass reads them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread: Vec<String>,
    /// A rebuild (forced, or for another model) started and has not finished a full pass; every
    /// pass resumes it until one does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rebuilding: bool,
    /// What a failed pass was to index, the whole tree (`retry_full`) or `retry_paths`: the next
    /// pass covers it too, and only a successful one clears it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry_full: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retry_paths: Vec<String>,
    /// The tokenizer whose lexical index was last kept in step with the items; each tokenizer
    /// has its own, left behind while another is in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lexical_tokenizer: Option<TokenizerKind>,
    /// Incremented on every save: a writer that finds another value than it last saw knows that
    /// another process wrote the index meanwhile.
    #[serde(default)]
    pub generation: u64,
    /// The lowest item id never allocated: ids are never reused, whichever tokenizer's lexical
    /// index (each keeps its own) was in use.
    #[serde(default)]
    pub next_id: u64,
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
