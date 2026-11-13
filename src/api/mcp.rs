//! The MCP server: three tools, `semantic_code_search`, `similar_code_search` and `status`,
//! served over HTTP here and over stdio by the `lode mcp` relay. Their names, descriptions and
//! inputs are part of the design (agents re-read them every turn); keep them verbatim.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::types::{DEFAULT_TOP_K, SearchRequest, SearchResponse};
use crate::registry::Engine;

pub const SEMANTIC_DESCRIPTION: &str = "Find existing code by what it does, when you do not know its name. Returns matching functions, types and macros as 'path:line:col  lines start-end  kind name(signature) — doc', where line:col is the name's position and start-end the item's line range (all 1-based). Use before writing any helper.";
pub const SIMILAR_DESCRIPTION: &str = "Find existing code that does the same job as given code: every function a diff adds, or the functions you consolidate, move or rewrite. Returns one section per `code` entry, listing the closest matches in semantic_code_search's format; the entry's own definition is not listed.";
/// Appended to the search tools' descriptions when [`crate::config::Config::is_local`].
pub const LOCAL_NOTE: &str = "It runs on this machine: the index and the embedding model are local, and nothing is sent elsewhere.";
pub const STATUS_DESCRIPTION: &str =
    "Index freshness for a repo: files indexed, pending changes, model, last update.";

/// How long a search waits for a new worktree's first index.
const FIRST_INDEX_WAIT: Duration = Duration::from_secs(30);

/// Normalized code shorter than this is never treated as the passed code's own definition.
const MIN_SELF_MATCH: usize = 24;

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Inputs shared by both search tools. No `top_k`: agents asked for 8 to 12 results, fewer than
/// the default, and missed answers ranked below that.
fn search_properties(first: Value) -> Value {
    let mut props = object(first);
    props.insert("repo".into(), repo_property());
    props.extend(object(json!({
        "from_unit": { "type": "string", "description": "a package, crate or module name from the repo's build graph: searches that unit and everything it may depend on. For only a unit's own files, use `paths`." },
        "language": { "type": "string", "description": "one language, e.g. rust, python, javascript, typescript, tsx, go, svelte or markdown; `text` for plain prose files. Case-insensitive; a name no item has returns nothing." },
        "paths": { "type": "array", "items": { "type": "string" }, "description": "repo-relative globs: `*` stays within a directory and `**` crosses them, a directory matches everything under it, and a leading `!` excludes, e.g. [\"crates/foo\", \"!**/generated/**\"]" },
        "include_tests": { "type": "boolean", "default": false, "description": "include test functions and test modules, ranked below other code" }
    })));
    Value::Object(props)
}

fn repo_property() -> Value {
    json!({ "type": "string", "description": "omit to use this session's worktree; otherwise a worktree root path or registered repo name, to use another repository" })
}

/// The MCP tools. All of them only read the index, which harnesses may use to approve them;
/// `local` (see [`crate::config::Config::is_local`]) adds [`LOCAL_NOTE`] to the searches.
pub fn tools(local: bool) -> Vec<Tool> {
    let read_only = ToolAnnotations::default().read_only(true).open_world(false);
    let search = |description: &str| {
        if local {
            format!("{description} {LOCAL_NOTE}")
        } else {
            description.to_string()
        }
    };
    [
        Tool::new(
            "semantic_code_search",
            search(SEMANTIC_DESCRIPTION),
            object(json!({
                "type": "object",
                "properties": search_properties(json!({
                    "query": { "type": "string", "description": "one sentence describing what the code does, e.g. \"parse a duration like 5m or 2h into seconds\"" },
                    "example_identifiers": { "type": "array", "items": { "type": "string" }, "description": "three to five guessed function or type names in the codebase's naming style (wrong guesses still help), e.g. [\"parse_duration\", \"duration_from_str\", \"to_secs\"]" }
                })),
                "required": ["query"]
            })),
        ),
        Tool::new(
            "similar_code_search",
            search(SIMILAR_DESCRIPTION),
            object(json!({
                "type": "object",
                "properties": search_properties(json!({
                    "code": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "the source of each function or type to check, one per entry, never a whole diff in one entry; pass all of them in one call. Diff `+` prefixes are stripped." }
                })),
                "required": ["code"]
            })),
        ),
        Tool::new(
            "status",
            STATUS_DESCRIPTION,
            object(json!({
                "type": "object",
                "properties": { "repo": repo_property() }
            })),
        ),
    ]
    .into_iter()
    .map(|t| t.annotate(read_only.clone()))
    .collect()
}

#[derive(Clone, Deserialize)]
struct Scope {
    repo: Option<String>,
    from_unit: Option<String>,
    language: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    include_tests: Option<bool>,
}

impl Scope {
    fn request(self, query: String) -> SearchRequest {
        SearchRequest {
            query,
            repo: self.repo,
            from_unit: self.from_unit,
            language: self.language,
            paths: self.paths,
            top_k: DEFAULT_TOP_K,
            include_tests: self.include_tests.unwrap_or(false),
            ..SearchRequest::default()
        }
    }
}

#[derive(Deserialize)]
struct SemanticArgs {
    query: String,
    #[serde(default)]
    example_identifiers: Vec<String>,
    #[serde(flatten)]
    scope: Scope,
}

#[derive(Deserialize)]
struct SimilarArgs {
    code: Definitions,
    #[serde(flatten)]
    scope: Scope,
}

/// `similar_code_search`'s `code`: a list, or (as before) a single string.
#[derive(Deserialize)]
#[serde(untagged)]
enum Definitions {
    One(String),
    Many(Vec<String>),
}

impl Definitions {
    fn into_vec(self) -> Vec<String> {
        match self {
            Definitions::One(c) => vec![c],
            Definitions::Many(cs) => cs,
        }
    }
}

/// Results per definition when several are checked at once: the single-definition count shared
/// out, but never fewer than this.
const MIN_RESULTS_EACH: usize = 5;

#[derive(Deserialize, Default)]
struct StatusArgs {
    repo: Option<String>,
}

/// The sentence followed by the guessed names: the query form the index was tuned on.
fn semantic_query(query: &str, identifiers: &[String]) -> String {
    let query = query.trim();
    if identifiers.is_empty() {
        return query.to_string();
    }
    let sep = if query.ends_with(['.', '!', '?']) {
        " "
    } else {
        ". "
    };
    format!("{query}{sep}{}", identifiers.join(" "))
}

/// Strips unified-diff markup when the code was pasted from a diff: drops removed lines and
/// hunk headers and the `+`/` ` prefix of the rest.
fn strip_diff(code: &str) -> String {
    let lines: Vec<&str> = code.lines().filter(|l| !l.trim().is_empty()).collect();
    let is_diff = lines.iter().any(|l| l.starts_with('+'))
        && lines
            .iter()
            .all(|l| l.starts_with(['+', '-', ' ']) || l.starts_with("@@"));
    if !is_diff {
        return code.to_string();
    }
    code.lines()
        .filter(|l| !l.starts_with('-') && !l.starts_with("@@"))
        .map(|l| l.get(1..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `code` without whitespace and `//` or `/* */` comments, which agents often drop when pasting.
fn squash(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut rest = code;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.find('\n').map_or("", |i| &after[i..]);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.find("*/").map_or("", |i| &after[i + 2..]);
        } else {
            let c = rest.chars().next().unwrap_or_default();
            if !c.is_whitespace() {
                out.push(c);
            }
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Whether a result is the passed code's own definition: one contains the other, ignoring
/// whitespace and comments.
fn is_self(code: &str, body: &str) -> bool {
    let body = squash(body);
    body.len() >= MIN_SELF_MATCH
        && code.len() >= MIN_SELF_MATCH
        && (body.contains(code) || code.contains(&body))
}

/// Drops the passed code's own definition from `resp` and keeps `top_k` results.
/// A definition's first non-empty line, shortened, to head its results.
fn first_line(code: &str) -> String {
    let line = code
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut out: String = line.chars().take(100).collect();
    if line.chars().count() > 100 {
        out.push('…');
    }
    out
}

fn drop_self(resp: &mut SearchResponse, code: &str, top_k: usize) {
    let code = squash(code);
    let mut left_out = Vec::new();
    resp.results.retain_mut(|r| {
        let own = r.body.take().is_some_and(|b| is_self(&code, &b));
        if own {
            left_out.push(format!("{}:{} {}", r.path, r.line, r.name));
        }
        !own
    });
    resp.results.truncate(top_k);
    if !left_out.is_empty() {
        resp.notes.push(format!(
            "left out the passed code itself: {}",
            left_out.join(", ")
        ));
    }
}

/// Called with the `repo` a search names (`None`: the default), before it runs.
pub type OnSearch = Arc<dyn Fn(Option<&str>) + Send + Sync>;

#[derive(Clone)]
pub struct LodestoneMcp {
    engine: Arc<Engine>,
    on_search: Option<OnSearch>,
}

impl LodestoneMcp {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            on_search: None,
        }
    }

    /// Lets the daemon keep searched worktrees watched and indexed.
    pub fn on_search(mut self, f: OnSearch) -> Self {
        self.on_search = Some(f);
        self
    }

    /// Starts indexing the searched worktree through `on_search`, and waits up to
    /// [`FIRST_INDEX_WAIT`] for a worktree with no index yet (a new agent worktree) to get one,
    /// rather than answer from an empty index.
    async fn searching(&self, scope: &Scope) {
        let Some(f) = &self.on_search else {
            return;
        };
        f(scope.repo.as_deref());
        let engine = self.engine.clone();
        let repo = scope.repo.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(w) = engine.resolve(repo.as_deref()) {
                w.await_first_index(FIRST_INDEX_WAIT);
            }
        })
        .await;
    }

    async fn search(
        &self,
        req: SearchRequest,
        post: impl FnOnce(&mut SearchResponse) + Send + 'static,
    ) -> Result<String, String> {
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || {
            engine.search(&req).map(|mut r| {
                post(&mut r);
                r.to_text()
            })
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
    }

    /// One search per definition; with several, a section each, headed by its first line.
    async fn similar(&self, scope: Scope, codes: Vec<String>) -> Result<String, String> {
        let codes: Vec<String> = codes
            .iter()
            .map(|c| strip_diff(c))
            .filter(|c| !c.trim().is_empty())
            .collect();
        if codes.is_empty() {
            return Err("`code` has no source to compare".into());
        }
        let each = (DEFAULT_TOP_K / codes.len()).max(MIN_RESULTS_EACH);
        let n = codes.len();
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || {
            let mut sections = Vec::new();
            for (i, code) in codes.into_iter().enumerate() {
                let mut req = scope.clone().request(code.clone());
                // Room for the passed code's own definition, which is dropped afterwards.
                req.top_k = each + 2;
                req.with_body = Some(usize::MAX);
                let mut resp = engine.search(&req).map_err(|e| format!("{e:#}"))?;
                drop_self(&mut resp, &code, each);
                sections.push(if n == 1 {
                    resp.to_text()
                } else {
                    format!(
                        "## {}/{n}: {}\n{}",
                        i + 1,
                        first_line(&code),
                        resp.to_text()
                    )
                });
            }
            Ok(sections.join("\n\n"))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Executes a tool call and returns its plain-text output, or an error message.
    pub async fn call(&self, name: &str, args: Map<String, Value>) -> Result<String, String> {
        let args = Value::Object(args);
        let invalid = |e: serde_json::Error| format!("invalid arguments: {e}");
        match name {
            "semantic_code_search" => {
                let a: SemanticArgs = serde_json::from_value(args).map_err(invalid)?;
                self.searching(&a.scope).await;
                let query = semantic_query(&a.query, &a.example_identifiers);
                self.search(a.scope.request(query), |_| {}).await
            }
            "similar_code_search" => {
                let a: SimilarArgs = serde_json::from_value(args).map_err(invalid)?;
                self.searching(&a.scope).await;
                self.similar(a.scope, a.code.into_vec()).await
            }
            "status" => {
                let a: StatusArgs = serde_json::from_value(args).unwrap_or_default();
                let engine = self.engine.clone();
                tokio::task::spawn_blocking(move || {
                    engine.status(a.repo.as_deref()).map(|s| s.to_text())
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("{e:#}"))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
}

/// Server identity shared by the HTTP server and the `lode mcp` relay.
pub fn server_info() -> ServerConfig {
    let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
    info.server_info.name = "lodestone".into();
    info.server_info.version = env!("CARGO_PKG_VERSION").into();
    info
}

impl ServerHandler for LodestoneMcp {
    fn get_info(&self) -> ServerConfig {
        server_info()
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools(
            self.engine.config.is_local(),
        )))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let result = match self
            .call(&request.name, request.arguments.unwrap_or_default())
            .await
        {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(msg) => CallToolResult::error(vec![ContentBlock::text(msg)]),
        };
        Ok(result.into())
    }
}

pub fn service(tools: LodestoneMcp) -> StreamableHttpService<LodestoneMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(tools.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::SearchResult;

    #[test]
    fn three_tools_with_plan_descriptions() {
        let t = tools(false);
        let names: Vec<&str> = t.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            ["semantic_code_search", "similar_code_search", "status"]
        );
        assert_eq!(t[0].description.as_deref(), Some(SEMANTIC_DESCRIPTION));
        assert_eq!(t[1].description.as_deref(), Some(SIMILAR_DESCRIPTION));
        assert_eq!(t[2].description.as_deref(), Some(STATUS_DESCRIPTION));
        let local = tools(true);
        assert_eq!(
            local[0].description.as_deref(),
            Some(format!("{SEMANTIC_DESCRIPTION} {LOCAL_NOTE}").as_str())
        );
        assert!(
            local[1]
                .description
                .as_deref()
                .unwrap()
                .ends_with(LOCAL_NOTE)
        );
        assert_eq!(local[2].description.as_deref(), Some(STATUS_DESCRIPTION));
        let keys = |i: usize| {
            let mut k: Vec<String> = t[i].input_schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            k.sort();
            k
        };
        let shared = ["from_unit", "include_tests", "language", "paths", "repo"];
        let with = |extra: &[&str]| {
            let mut k: Vec<String> = shared.iter().chain(extra).map(|s| s.to_string()).collect();
            k.sort();
            k
        };
        assert_eq!(keys(0), with(&["example_identifiers", "query"]));
        assert_eq!(keys(1), with(&["code"]));
        // Each input's instructions live on the input itself.
        for tool in &t {
            for (key, prop) in tool.input_schema["properties"].as_object().unwrap() {
                let d = prop["description"].as_str().unwrap_or_default();
                assert!(!d.is_empty(), "{}.{key} has no description", tool.name);
            }
        }
        assert_eq!(t[0].input_schema["required"], json!(["query"]));
        assert_eq!(t[1].input_schema["required"], json!(["code"]));
    }

    #[test]
    fn semantic_query_appends_identifiers() {
        let ids = vec!["parse_duration".to_string(), "to_secs".to_string()];
        assert_eq!(
            semantic_query("parse a duration", &ids),
            "parse a duration. parse_duration to_secs"
        );
        assert_eq!(
            semantic_query("parse a duration. ", &ids),
            "parse a duration. parse_duration to_secs"
        );
        assert_eq!(semantic_query("parse a duration", &[]), "parse a duration");
    }

    #[test]
    fn strip_diff_only_touches_diffs() {
        let diff = "@@ -1,2 +1,3 @@\n+fn add(a: u32) -> u32 {\n-    old\n+    a + 1\n }";
        assert_eq!(strip_diff(diff), "fn add(a: u32) -> u32 {\n    a + 1\n}");
        let code = "fn add(a: u32) -> u32 {\n    +a\n}";
        assert_eq!(strip_diff(code), code);
    }

    fn result(name: &str, body: &str) -> SearchResult {
        SearchResult {
            path: "src/lib.rs".into(),
            line: 1,
            name: name.into(),
            body: Some(body.into()),
            ..SearchResult::default()
        }
    }

    #[test]
    fn squash_ignores_whitespace_and_comments() {
        let commented = "fn f() {\n    // why\n    let x = 1; /* note */\n    x\n}";
        assert_eq!(squash(commented), "fnf(){letx=1;x}");
        assert_eq!(squash("a /* open"), "a");
    }

    #[test]
    fn drop_self_removes_the_passed_definition_only() {
        let own = "fn compact_size(n: u64, out: &mut Vec<u8>) {\n    // one byte\n    out.push(n as u8);\n}";
        let mut resp = SearchResponse {
            results: vec![
                result("compact_size", own),
                result(
                    "write_varint",
                    "fn write_varint(n: u64, w: &mut Vec<u8>) { w.push(n as u8) }",
                ),
                result("tiny", "fn t() {}"),
            ],
            ..SearchResponse::default()
        };
        // Re-indented and without its comment, it is still the same definition.
        let pasted = "fn compact_size(n: u64, out: &mut Vec<u8>) { out.push(n as u8); }";
        drop_self(&mut resp, pasted, 1);
        let names: Vec<&str> = resp.results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["write_varint"]);
        assert!(resp.results.iter().all(|r| r.body.is_none()));
        assert_eq!(
            resp.notes,
            ["left out the passed code itself: src/lib.rs:1 compact_size"]
        );
    }
}
