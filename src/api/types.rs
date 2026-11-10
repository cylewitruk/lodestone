//! Public request/response types shared by REST, MCP and the CLI.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::index::{IndexReport, Progress};

/// Results per search when the caller does not ask for a number (MCP, REST, CLI).
pub const DEFAULT_TOP_K: usize = 24;

fn default_top_k() -> usize {
    DEFAULT_TOP_K
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchRequest {
    #[serde(alias = "q")]
    pub query: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub from_unit: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default)]
    pub include_tests: bool,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Lines of source to include per result (REST only).
    #[serde(default)]
    pub with_body: Option<usize>,
}

/// Takes or renews an agent session's lease (`POST /v1/sessions`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionRequest {
    pub id: String,
    /// The session's working directory; leases the registered worktree containing it.
    pub cwd: String,
    pub ttl_secs: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionResponse {
    /// The registered worktree the session works in, if any.
    pub repo: Option<RepoInfo>,
}

/// An MCP tool call relayed by `lode mcp` (`POST /v1/tool`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolRequest {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Map<String, serde_json::Value>,
    /// Lease id; a call without `repo` searches the session's worktree.
    #[serde(default)]
    pub session: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    pub path: String,
    /// First line of the definition (1-based).
    pub line: u32,
    /// Last line of the definition (inclusive).
    #[serde(default)]
    pub line_end: u32,
    /// 1-based line and character column of the name (0 when the index predates them).
    #[serde(default)]
    pub name_line: u32,
    #[serde(default)]
    pub name_column: u32,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub signature: String,
    pub doc: String,
    pub unit: Option<String>,
    pub language: String,
    pub is_test: bool,
    pub score: f32,
    pub similarity: Option<f32>,
    /// The formatted output line.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchResponse {
    pub repo: String,
    pub stale: bool,
    pub stale_reasons: Vec<String>,
    pub notes: Vec<String>,
    pub results: Vec<SearchResult>,
    pub took_ms: f64,
}

impl SearchResponse {
    /// Plain-text rendering used by MCP and the CLI: one line per item.
    pub fn to_text(&self) -> String {
        let mut out: Vec<String> = Vec::new();
        for r in &self.results {
            out.push(r.text.clone());
            if let Some(b) = &r.body {
                out.extend(b.lines().map(|l| format!("    {l}")));
            }
        }
        if self.results.is_empty() {
            out.push("no matches".into());
        }
        for n in &self.notes {
            out.push(format!("# note: {n}"));
        }
        if self.stale {
            out.push(format!("# stale: {}", self.stale_reasons.join("; ")));
        }
        out.join("\n")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ExtensionCount {
    pub tagged: u64,
    pub text: u64,
    pub skipped_files: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StatusReport {
    pub repo: String,
    pub name: String,
    pub worktree_id: String,
    pub model: String,
    pub backend: String,
    pub index_model: String,
    pub files_indexed: usize,
    pub items: usize,
    pub vectors: usize,
    #[serde(default)]
    pub doc_vectors: usize,
    pub pending_changes: usize,
    pub last_update: u64,
    pub stale: bool,
    pub stale_reasons: Vec<String>,
    pub indexing: Progress,
    pub last_report: Option<IndexReport>,
    pub graph_providers: Vec<String>,
    pub units: usize,
    pub layer_filter: bool,
    pub watcher: String,
    pub extensions: BTreeMap<String, ExtensionCount>,
}

impl StatusReport {
    /// `key: value` lines.
    pub fn to_text(&self) -> String {
        let mut l = vec![
            format!("repo: {}", self.repo),
            format!("name: {}", self.name),
            format!("model: {} ({})", self.model, self.backend),
            format!("files_indexed: {}", self.files_indexed),
            format!("items: {}", self.items),
            format!("vectors: {}", self.vectors),
            format!("doc_vectors: {}", self.doc_vectors),
            format!("pending_changes: {}", self.pending_changes),
            format!("last_update: {}", fmt_time(self.last_update)),
            format!("stale: {}", self.stale),
        ];
        if self.index_model != self.model && !self.index_model.is_empty() {
            l.push(format!("index_model: {}", self.index_model));
        }
        for r in &self.stale_reasons {
            l.push(format!("stale_reason: {r}"));
        }
        if self.indexing.running {
            let p = &self.indexing;
            l.push(format!(
                "indexing: files {}/{} embedded {} cached {} eta {}s",
                p.files_done,
                p.files_total,
                p.chunks_embedded,
                p.chunks_cached,
                p.eta_secs.unwrap_or(0)
            ));
        }
        if let Some(r) = &self.last_report {
            l.push(format!(
                "last_index: {} changed, {} removed, {} embedded, {} cached, {:.2}s",
                r.files_changed, r.files_removed, r.chunks_embedded, r.chunks_cached, r.secs
            ));
        }
        l.push(format!(
            "graph: {} ({} units)",
            self.graph_providers.join(", "),
            self.units
        ));
        if !self.layer_filter {
            l.push("layer_filter: off (no build-graph provider matched)".into());
        }
        l.push(format!("watcher: {}", self.watcher));
        for (ext, c) in &self.extensions {
            l.push(format!(
                ".{ext}: tagged {} text {} skipped_files {}",
                c.tagged, c.text, c.skipped_files
            ));
        }
        l.join("\n")
    }
}

fn fmt_time(secs: u64) -> String {
    if secs == 0 {
        return "never".into();
    }
    let ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(secs))
        .unwrap_or(0);
    format!("{secs} ({ago}s ago)")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RegisterRequest {
    pub path: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexRequest {
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexResponse {
    pub repo: String,
    pub report: IndexReport,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RepoInfo {
    pub root: String,
    pub name: String,
    pub worktree_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;

    fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: T) {
        let json = serde_json::to_string(&v).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn serde_roundtrips() {
        roundtrip(SearchRequest {
            query: "q".into(),
            top_k: DEFAULT_TOP_K,
            kinds: vec!["function".into()],
            ..Default::default()
        });
        roundtrip(SearchResult {
            path: "a.rs".into(),
            line: 3,
            line_end: 9,
            name_line: 3,
            name_column: 8,
            similarity: Some(0.5),
            body: Some("x".into()),
            ..Default::default()
        });
        roundtrip(SearchResponse {
            repo: "r".into(),
            stale: true,
            results: vec![SearchResult::default()],
            ..Default::default()
        });
        let mut st = StatusReport {
            repo: "r".into(),
            ..Default::default()
        };
        st.extensions.insert(
            "rs".into(),
            ExtensionCount {
                tagged: 1,
                text: 2,
                skipped_files: 3,
            },
        );
        roundtrip(st);
        roundtrip(ExtensionCount::default());
        roundtrip(RegisterRequest { path: "/x".into() });
        roundtrip(IndexRequest {
            repo: Some("r".into()),
            force: true,
        });
        roundtrip(IndexResponse {
            repo: "r".into(),
            report: IndexReport::default(),
        });
        roundtrip(RepoInfo {
            root: "/r".into(),
            name: "r".into(),
            worktree_id: "abc".into(),
        });
        roundtrip(ErrorBody { error: "e".into() });
    }

    #[test]
    fn search_request_defaults() {
        let r: SearchRequest = serde_json::from_str(r#"{"query":"x"}"#).unwrap();
        assert_eq!(r.top_k, DEFAULT_TOP_K);
        assert!(!r.include_tests);
    }
}
