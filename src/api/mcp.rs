//! The MCP server: exactly two tools, `search` and `status`. Their names, descriptions and
//! inputs are part of the design (agents re-read them every turn); keep them verbatim.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::types::{DEFAULT_TOP_K, SearchRequest};
use crate::registry::Engine;

pub const SEARCH_DESCRIPTION: &str = "Find existing code by what it does. Describe the behaviour in plain words (not a guessed name); returns matching functions, types and macros as 'path:line kind name(signature) — doc'. Use before writing any helper.";
pub const STATUS_DESCRIPTION: &str =
    "Index freshness for a repo: files indexed, pending changes, model, last update.";

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// The two tool definitions.
pub fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "search",
            SEARCH_DESCRIPTION,
            object(json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "repo": { "type": "string", "description": "worktree root or registered name; default = the only registered repo, else error" },
                    "from_unit": { "type": "string", "description": "only items this package/crate/module may depend on, per the repo's build graph" },
                    "language": { "type": "string", "description": "restrict to one language" },
                    "top_k": { "type": "integer", "default": DEFAULT_TOP_K, "maximum": 50 },
                    "include_tests": { "type": "boolean", "default": false }
                },
                "required": ["query"]
            })),
        ),
        Tool::new(
            "status",
            STATUS_DESCRIPTION,
            object(json!({
                "type": "object",
                "properties": { "repo": { "type": "string" } }
            })),
        ),
    ]
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    repo: Option<String>,
    from_unit: Option<String>,
    language: Option<String>,
    top_k: Option<usize>,
    include_tests: Option<bool>,
}

#[derive(Deserialize, Default)]
struct StatusArgs {
    repo: Option<String>,
}

#[derive(Clone)]
pub struct LodestoneMcp {
    engine: Arc<Engine>,
}

impl LodestoneMcp {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    /// Executes a tool call and returns its plain-text output, or an error message.
    pub async fn call(&self, name: &str, args: Map<String, Value>) -> Result<String, String> {
        let engine = self.engine.clone();
        let args = Value::Object(args);
        match name {
            "search" => {
                let a: SearchArgs =
                    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
                let req = SearchRequest {
                    query: a.query,
                    repo: a.repo,
                    from_unit: a.from_unit,
                    language: a.language,
                    top_k: a.top_k.unwrap_or(DEFAULT_TOP_K).clamp(1, 50),
                    include_tests: a.include_tests.unwrap_or(false),
                    ..SearchRequest::default()
                };
                tokio::task::spawn_blocking(move || engine.search(&req).map(|r| r.to_text()))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("{e:#}"))
            }
            "status" => {
                let a: StatusArgs = serde_json::from_value(args).unwrap_or_default();
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

impl ServerHandler for LodestoneMcp {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info.name = "lodestone".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
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

pub fn service(engine: Arc<Engine>) -> StreamableHttpService<LodestoneMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(LodestoneMcp::new(engine.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_two_tools_with_plan_descriptions() {
        let t = tools();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].name, "search");
        assert_eq!(t[0].description.as_deref(), Some(SEARCH_DESCRIPTION));
        assert_eq!(t[1].name, "status");
        assert_eq!(t[1].description.as_deref(), Some(STATUS_DESCRIPTION));
        let props = t[0].input_schema["properties"].as_object().unwrap();
        let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "from_unit",
                "include_tests",
                "language",
                "query",
                "repo",
                "top_k"
            ]
        );
        assert_eq!(t[0].input_schema["required"], json!(["query"]));
    }
}
