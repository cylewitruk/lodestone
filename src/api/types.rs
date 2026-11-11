//! Public request/response types shared by REST, MCP and the CLI.

use std::collections::BTreeMap;
use std::iter;

use serde::{Deserialize, Serialize};

use crate::index::{IndexReport, Progress};
use crate::text;

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
            // An index still being built answers with nothing, not with "nothing exists".
            let building = self
                .stale_reasons
                .iter()
                .any(|r| r.starts_with("not indexed yet") || r.starts_with("pending:"));
            out.push(if building {
                "no results yet: this worktree is still being indexed; search again shortly".into()
            } else {
                "no matches".into()
            });
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

impl ExtensionCount {
    /// Items indexed from files with this extension.
    pub fn items(&self) -> u64 {
        self.tagged + self.text
    }
}

impl StatusReport {
    /// Grouped `key  value` sections, then the per-extension table.
    pub fn to_text(&self) -> String {
        self.text_at(text::now())
    }

    fn text_at(&self, now: u64) -> String {
        let mut repository = vec![
            ("name", self.name.clone()),
            ("path", self.repo.clone()),
            ("model", format!("{} ({})", self.model, self.backend)),
        ];
        if self.index_model != self.model && !self.index_model.is_empty() {
            repository.push(("index model", self.index_model.clone()));
        }
        let index = vec![
            ("files", text::count(self.files_indexed)),
            ("items", text::count(self.items)),
            ("vectors", text::count(self.vectors)),
            ("doc vectors", text::count(self.doc_vectors)),
            ("pending", text::count(self.pending_changes)),
        ];
        let mut freshness = vec![
            ("updated", text::ago(self.last_update, now)),
            ("state", self.state().into()),
        ];
        freshness.extend(self.stale_reasons.iter().map(|r| ("reason", r.clone())));
        if self.indexing.running {
            let p = &self.indexing;
            freshness.push((
                "indexing",
                format!(
                    "files {}/{}, {} embedded, {} cached, eta {}s",
                    text::count(p.files_done),
                    text::count(p.files_total),
                    text::count(p.chunks_embedded),
                    text::count(p.chunks_cached),
                    p.eta_secs.unwrap_or(0)
                ),
            ));
        }
        if let Some(r) = &self.last_report {
            freshness.push((
                "last run",
                format!(
                    "{} changed, {} removed, {} embedded, {} cached in {:.2}s",
                    text::count(r.files_changed),
                    text::count(r.files_removed),
                    text::count(r.chunks_embedded),
                    text::count(r.chunks_cached),
                    r.secs
                ),
            ));
        }
        freshness.push(("watcher", self.watcher.clone()));
        let graph = vec![
            ("providers", self.graph_providers.join(", ")),
            ("units", text::count(self.units)),
            (
                "layer filter",
                if self.layer_filter {
                    "on".into()
                } else {
                    "off (no build-graph provider matched)".into()
                },
            ),
        ];
        let sections = [
            ("repository", repository),
            ("index", index),
            ("freshness", freshness),
            ("graph", graph),
        ];
        let key = sections
            .iter()
            .flat_map(|(_, rows)| rows.iter().map(|(k, _)| k.len()))
            .max()
            .unwrap_or(0);
        let mut out: Vec<String> = sections
            .iter()
            .map(|(title, rows)| {
                let rows = rows.iter().map(|(k, v)| format!("  {k:<key$}  {v}"));
                iter::once(title.to_string())
                    .chain(rows)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect();
        if !self.extensions.is_empty() {
            out.push(self.extensions_text());
        }
        out.join("\n\n")
    }

    /// `indexing`, `stale` or `fresh`.
    pub fn state(&self) -> &'static str {
        if self.indexing.running {
            "indexing"
        } else if self.stale {
            "stale"
        } else {
            "fresh"
        }
    }

    /// Extensions with items as a table, most items first; those without on one line.
    fn extensions_text(&self) -> String {
        let (mut with_items, without): (Vec<_>, Vec<_>) =
            self.extensions.iter().partition(|(_, c)| c.items() > 0);
        with_items.sort_by(|(a, x), (b, y)| y.items().cmp(&x.items()).then(a.cmp(b)));
        // Files without one are counted under `(none)`.
        let label = |ext: &str| {
            if ext.starts_with('(') {
                ext.to_string()
            } else {
                format!(".{ext}")
            }
        };
        let header = (
            String::new(),
            ["items", "tagged", "text", "skipped files"]
                .map(String::from)
                .to_vec(),
        );
        let rows: Vec<(String, Vec<String>)> = iter::once(header)
            .chain(with_items.into_iter().map(|(ext, c)| {
                (
                    label(ext),
                    vec![
                        text::count(c.items()),
                        text::count(c.tagged),
                        text::count(c.text),
                        text::count(c.skipped_files),
                    ],
                )
            }))
            .collect();
        let mut out = format!("extensions\n{}", text::table(&rows));
        if !without.is_empty() {
            let skipped: u64 = without.iter().map(|(_, c)| c.skipped_files).sum();
            let exts: Vec<String> = without.iter().map(|(ext, _)| label(ext)).collect();
            out.push_str(&format!(
                "\n  no items: {} ({} skipped files)",
                exts.join(" "),
                text::count(skipped)
            ));
        }
        out
    }
}

/// `lode status` across every registered repository.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StatusOverview {
    pub repos: Vec<StatusReport>,
    /// Registered repositories whose status could not be read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<RepoFailure>,
    pub totals: StatusTotals,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RepoFailure {
    pub name: String,
    pub error: String,
}

/// Sums over `StatusOverview::repos`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StatusTotals {
    pub repos: usize,
    pub files_indexed: usize,
    pub items: usize,
    pub vectors: usize,
    pub doc_vectors: usize,
    pub pending_changes: usize,
    pub stale: usize,
}

impl StatusOverview {
    pub fn new(repos: Vec<StatusReport>, failed: Vec<RepoFailure>) -> Self {
        let totals = repos.iter().fold(
            StatusTotals {
                repos: repos.len(),
                ..Default::default()
            },
            |t, r| StatusTotals {
                files_indexed: t.files_indexed + r.files_indexed,
                items: t.items + r.items,
                vectors: t.vectors + r.vectors,
                doc_vectors: t.doc_vectors + r.doc_vectors,
                pending_changes: t.pending_changes + r.pending_changes,
                stale: t.stale + usize::from(r.stale),
                ..t
            },
        );
        Self {
            repos,
            failed,
            totals,
        }
    }

    /// The shared model, one row per repository and a total row.
    pub fn to_text(&self) -> String {
        self.text_at(text::now())
    }

    fn text_at(&self, now: u64) -> String {
        if self.repos.is_empty() && self.failed.is_empty() {
            return "no repositories registered; run `lode register <path>`".into();
        }
        let mut out = Vec::new();
        if let Some(r) = self.repos.first() {
            out.push(format!("model  {} ({})\n", r.model, r.backend));
        }
        let header = (
            "repository".to_string(),
            ["files", "items", "vectors", "pending", "updated", "state"]
                .map(String::from)
                .to_vec(),
        );
        let t = &self.totals;
        let total = (
            "total".to_string(),
            vec![
                text::count(t.files_indexed),
                text::count(t.items),
                text::count(t.vectors),
                text::count(t.pending_changes),
                String::new(),
                format!("{} stale", t.stale),
            ],
        );
        let rows: Vec<(String, Vec<String>)> = iter::once(header)
            .chain(self.repos.iter().map(|r| {
                (
                    r.name.clone(),
                    vec![
                        text::count(r.files_indexed),
                        text::count(r.items),
                        text::count(r.vectors),
                        text::count(r.pending_changes),
                        text::ago(r.last_update, now),
                        r.state().into(),
                    ],
                )
            }))
            .chain(iter::once(total))
            .collect();
        out.push(text::table(&rows));
        for f in &self.failed {
            out.push(format!("  {}: {}", f.name, f.error));
        }
        out.push("\n`lode status --repo <name>` shows one repository in detail".into());
        out.join("\n")
    }
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

    const NOW: u64 = 1_800_000_000;

    fn report(name: &str, files: usize, items: usize, stale: bool) -> StatusReport {
        StatusReport {
            repo: format!("/src/{name}"),
            name: name.into(),
            model: "jina-v2-code".into(),
            backend: "candle-metal".into(),
            index_model: "jina-v2-code".into(),
            files_indexed: files,
            items,
            vectors: items,
            last_update: NOW - 391,
            stale,
            watcher: "watching".into(),
            ..Default::default()
        }
    }

    fn ext(tagged: u64, text: u64, skipped_files: u64) -> ExtensionCount {
        ExtensionCount {
            tagged,
            text,
            skipped_files,
        }
    }

    #[test]
    fn status_text_groups_fields_and_tabulates_extensions() {
        let mut st = StatusReport {
            doc_vectors: 12_004,
            pending_changes: 3,
            stale_reasons: vec!["3 files changed since last index".into()],
            last_report: Some(IndexReport {
                files_changed: 12,
                files_removed: 1,
                chunks_embedded: 1_234,
                chunks_cached: 56,
                secs: 3.214,
                ..Default::default()
            }),
            graph_providers: vec!["fallback".into()],
            units: 1,
            ..report("stacks-core", 4_312, 58_210, true)
        };
        st.extensions = BTreeMap::from([
            ("(none)".into(), ext(0, 0, 1)),
            ("json".into(), ext(0, 0, 12)),
            ("lock".into(), ext(0, 0, 3)),
            ("md".into(), ext(0, 1_200, 0)),
            ("py".into(), ext(4_356, 0, 0)),
            ("rs".into(), ext(40_000, 0, 2)),
        ]);
        let expected = "\
repository
  name          stacks-core
  path          /src/stacks-core
  model         jina-v2-code (candle-metal)

index
  files         4,312
  items         58,210
  vectors       58,210
  doc vectors   12,004
  pending       3

freshness
  updated       6m 31s ago
  state         stale
  reason        3 files changed since last index
  last run      12 changed, 1 removed, 1,234 embedded, 56 cached in 3.21s
  watcher       watching

graph
  providers     fallback
  units         1
  layer filter  off (no build-graph provider matched)

extensions
        items  tagged   text  skipped files
  .rs  40,000  40,000      0              2
  .py   4,356   4,356      0              0
  .md   1,200       0  1,200              0
  no items: (none) .json .lock (16 skipped files)";
        assert_eq!(st.text_at(NOW), expected);
    }

    #[test]
    fn status_text_shows_indexing_progress_and_a_different_index_model() {
        let st = StatusReport {
            index_model: "old-model".into(),
            last_update: 0,
            indexing: Progress {
                running: true,
                files_done: 1_500,
                files_total: 4_000,
                chunks_embedded: 2_000,
                chunks_cached: 10,
                eta_secs: Some(42),
            },
            ..report("r", 0, 0, true)
        };
        let text = st.text_at(NOW);
        assert!(text.contains("\n  index model   old-model\n"), "{text}");
        assert!(text.contains("\n  updated       never\n"), "{text}");
        assert!(text.contains("\n  state         indexing\n"), "{text}");
        assert!(
            text.contains(
                "\n  indexing      files 1,500/4,000, 2,000 embedded, 10 cached, eta 42s\n"
            ),
            "{text}"
        );
        assert!(!text.contains("extensions"), "{text}");
    }

    #[test]
    fn overview_sums_repositories_and_renders_one_row_each() {
        let overview = StatusOverview::new(
            vec![
                StatusReport {
                    pending_changes: 3,
                    doc_vectors: 7,
                    ..report("stacks-core", 4_312, 58_210, true)
                },
                StatusReport {
                    last_update: NOW - 2 * 3_600 - 240,
                    ..report("concurro", 812, 9_100, false)
                },
            ],
            vec![RepoFailure {
                name: "gone".into(),
                error: "no such directory: /src/gone".into(),
            }],
        );
        assert_eq!(
            overview.totals,
            StatusTotals {
                repos: 2,
                files_indexed: 5_124,
                items: 67_310,
                vectors: 67_310,
                doc_vectors: 7,
                pending_changes: 3,
                stale: 1,
            }
        );
        let expected = "\
model  jina-v2-code (candle-metal)

  repository   files   items  vectors  pending     updated    state
  stacks-core  4,312  58,210   58,210        3  6m 31s ago    stale
  concurro       812   9,100    9,100        0   2h 4m ago    fresh
  total        5,124  67,310   67,310        3              1 stale
  gone: no such directory: /src/gone

`lode status --repo <name>` shows one repository in detail";
        assert_eq!(overview.text_at(NOW), expected);
        roundtrip(overview);
    }

    #[test]
    fn overview_of_nothing_says_how_to_register() {
        let overview = StatusOverview::new(Vec::new(), Vec::new());
        assert_eq!(overview.totals, StatusTotals::default());
        assert!(overview.to_text().contains("lode register"));
    }
}
