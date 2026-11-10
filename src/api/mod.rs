//! HTTP surface: exactly two MCP tools at `POST /mcp` and the `/v1` REST API, both over one
//! engine.

pub mod mcp;
pub mod types;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::config::Config;
use crate::index::IndexOptions;
use crate::registry::Engine;
use crate::watch::Watchers;
use types::{ErrorBody, IndexRequest, IndexResponse, RegisterRequest, RepoInfo, SearchRequest};

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub watchers: Arc<Watchers>,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    repo: Option<String>,
    from_unit: Option<String>,
    language: Option<String>,
    top_k: Option<usize>,
    include_tests: Option<bool>,
    path_prefix: Option<String>,
    with_body: Option<usize>,
    format: Option<String>,
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(ApiError::from)
}

async fn search_get(
    State(s): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Response> {
    let text = q.format.as_deref() == Some("text");
    let req = SearchRequest {
        query: q.q,
        repo: q.repo,
        from_unit: q.from_unit,
        language: q.language,
        top_k: q.top_k.unwrap_or(8),
        include_tests: q.include_tests.unwrap_or(false),
        path_prefix: q.path_prefix,
        kinds: Vec::new(),
        with_body: q.with_body,
    };
    let engine = s.engine.clone();
    let resp = blocking(move || engine.search(&req)).await?;
    Ok(if text {
        resp.to_text().into_response()
    } else {
        Json(resp).into_response()
    })
}

async fn search_post(
    State(s): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> ApiResult<Response> {
    let engine = s.engine.clone();
    Ok(Json(blocking(move || engine.search(&req)).await?).into_response())
}

#[derive(Deserialize)]
struct StatusQuery {
    repo: Option<String>,
    format: Option<String>,
}

async fn status(State(s): State<AppState>, Query(q): Query<StatusQuery>) -> ApiResult<Response> {
    let engine = s.engine.clone();
    let text = q.format.as_deref() == Some("text");
    let st = blocking(move || engine.status(q.repo.as_deref())).await?;
    Ok(if text {
        st.to_text().into_response()
    } else {
        Json(st).into_response()
    })
}

async fn register(
    State(s): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<RepoInfo>> {
    let w = s.engine.register(Path::new(&req.path))?;
    s.watchers.watch(&s.engine, w.clone());
    s.watchers
        .spawn_index(&s.engine, w.clone(), IndexOptions::default());
    Ok(Json(RepoInfo {
        root: w.root.to_string_lossy().to_string(),
        name: s.engine.repo_name(&w.root),
        worktree_id: w.id.clone(),
    }))
}

async fn index(
    State(s): State<AppState>,
    Json(req): Json<IndexRequest>,
) -> ApiResult<Json<IndexResponse>> {
    let w = match &req.repo {
        Some(r) if Path::new(r).is_dir() => s.engine.register(Path::new(r))?,
        r => s.engine.resolve(r.as_deref())?,
    };
    s.watchers.watch(&s.engine, w.clone());
    let ctx = Arc::new(s.engine.index_context()?);
    let report = w
        .index(
            ctx,
            IndexOptions {
                force: req.force,
                paths: None,
            },
        )
        .await?;
    Ok(Json(IndexResponse {
        repo: w.root.to_string_lossy().to_string(),
        report,
    }))
}

async fn repos(State(s): State<AppState>) -> Json<Vec<RepoInfo>> {
    Json(s.engine.repos())
}

pub fn router(state: AppState) -> Router {
    let mcp = mcp::service(state.engine.clone());
    Router::new()
        .route("/v1/search", get(search_get).post(search_post))
        .route("/v1/status", get(status))
        .route("/v1/register", post(register))
        .route("/v1/index", post(index))
        .route("/v1/repos", get(repos))
        .nest_service("/mcp", mcp)
        .with_state(state)
}

/// Runs the daemon until interrupted: loads the model, opens every registered worktree, starts
/// watchers and catch-up indexing, then serves on 127.0.0.1.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let port = config.port;
    let engine = Arc::new(tokio::task::spawn_blocking(move || Engine::from_env(config)).await??);
    {
        let e = engine.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            e.embedder()?
                .embed(&["warm up"], crate::embed::InputKind::Query)?;
            if let Some(r) = e.reranker() {
                r.score("warm up", &["fn warm_up() {}"])?;
            }
            Ok(())
        })
        .await??;
    }
    let watchers = Arc::new(Watchers::default());
    for repo in engine.repos() {
        match engine.open_canonical(Path::new(&repo.root)) {
            Ok(w) => {
                watchers.watch(&engine, w.clone());
                watchers.spawn_index(&engine, w, IndexOptions::default());
            }
            Err(e) => tracing::warn!("cannot open {}: {e:#}", repo.root),
        }
    }
    let app = router(AppState { engine, watchers });
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("lodestone listening on http://{addr} (MCP at /mcp)");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
