//! HTTP surface: the MCP tools at `POST /mcp` and the `/v1` REST API, both over one engine.

mod listen;
pub mod mcp;
pub mod sessions;
pub mod types;

use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::config::Config;
use crate::index::{IndexOptions, WorktreeIndex};
use crate::registry::{self, Engine};
use crate::watch::Watchers;
use listen::Listeners;
use mcp::LodestoneMcp;
use sessions::Leases;
use types::{
    DEFAULT_TOP_K, ErrorBody, IndexRequest, IndexResponse, RegisterRequest, RepoInfo,
    SearchRequest, SessionRequest, SessionResponse, ToolRequest,
};

/// How long a request that names a repository keeps it watched by an on-demand daemon (at most
/// the idle timeout).
const REQUEST_LEASE: Duration = Duration::from_secs(600);
/// How often an on-demand daemon expires leases and checks for idleness.
const SWEEP: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub watchers: Arc<Watchers>,
    pub leases: Arc<Mutex<Leases>>,
    /// Watch only leased worktrees (an auto-started daemon); otherwise every registered one.
    pub on_demand: bool,
    pub request_lease: Duration,
}

impl AppState {
    fn leases(&self) -> std::sync::MutexGuard<'_, Leases> {
        self.leases.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Watches `w` and catches it up, unless it is already watched.
    fn activate(&self, w: Arc<WorktreeIndex>) {
        if !self.watchers.is_watched(&w.root) {
            self.watchers
                .spawn_index(&self.engine, w, IndexOptions::default());
        }
    }

    /// Keeps a worktree a request names watched for a while (on-demand daemons only).
    fn lease_request(&self, repo: Option<&str>) {
        if !self.on_demand {
            return;
        }
        if let Ok(w) = self.engine.resolve(repo) {
            self.leases().renew(
                &format!("request:{}", w.root.display()),
                Some(w.root.clone()),
                self.request_lease,
                Instant::now(),
            );
            self.activate(w);
        }
    }
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
        top_k: q.top_k.unwrap_or(DEFAULT_TOP_K),
        include_tests: q.include_tests.unwrap_or(false),
        path_prefix: q.path_prefix,
        kinds: Vec::new(),
        with_body: q.with_body,
    };
    s.lease_request(req.repo.as_deref());
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
    s.lease_request(req.repo.as_deref());
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
    s.watchers
        .spawn_index(&s.engine, w.clone(), IndexOptions::default());
    s.lease_request(Some(&w.root.to_string_lossy()));
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
    s.lease_request(Some(&w.root.to_string_lossy()));
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

/// Takes or renews a session's lease on the registered worktree containing its `cwd`.
async fn session_renew(
    State(s): State<AppState>,
    Json(req): Json<SessionRequest>,
) -> ApiResult<Json<SessionResponse>> {
    let repos = s.engine.repos();
    let roots: Vec<PathBuf> = repos.iter().map(|r| PathBuf::from(&r.root)).collect();
    let root = registry::root_of(&roots, Path::new(&req.cwd));
    s.leases().renew(
        &req.id,
        root.clone(),
        Duration::from_secs(req.ttl_secs),
        Instant::now(),
    );
    if let Some(root) = &root {
        s.activate(s.engine.open_canonical(root)?);
    }
    let repo = root.and_then(|r| repos.into_iter().find(|i| Path::new(&i.root) == r));
    Ok(Json(SessionResponse { repo }))
}

async fn session_release(State(s): State<AppState>, UrlPath(id): UrlPath<String>) -> StatusCode {
    s.leases().release(&id, Instant::now());
    StatusCode::NO_CONTENT
}

/// Runs an MCP tool for `lode mcp`; a call without `repo` searches the session's worktree.
async fn tool(State(s): State<AppState>, Json(mut req): Json<ToolRequest>) -> ApiResult<String> {
    if !req.arguments.contains_key("repo")
        && let Some(root) = req.session.as_deref().and_then(|id| s.leases().root(id))
    {
        req.arguments
            .insert("repo".into(), root.to_string_lossy().into());
    }
    LodestoneMcp::new(s.engine.clone())
        .call(&req.name, req.arguments)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))
}

pub fn router(state: AppState) -> Router {
    let mcp = mcp::service(state.engine.clone());
    Router::new()
        .route("/v1/search", get(search_get).post(search_post))
        .route("/v1/status", get(status))
        .route("/v1/register", post(register))
        .route("/v1/index", post(index))
        .route("/v1/repos", get(repos))
        .route("/v1/sessions", post(session_renew))
        .route("/v1/sessions/{id}", axum::routing::delete(session_release))
        .route("/v1/tool", post(tool))
        .nest_service("/mcp", mcp)
        .with_state(state)
}

/// Runs the daemon until interrupted. It binds first, so a second daemon started at the same
/// time exits at once; then it loads the model and starts watching. With `on_demand` (a daemon
/// started by `lode mcp` or the session hook) it watches only worktrees agent sessions lease and
/// exits `idle_timeout_secs` after the last lease ends; otherwise it watches every registered
/// worktree and runs until interrupted.
pub async fn serve(config: Config, on_demand: bool) -> anyhow::Result<()> {
    // Before the model loads, so a second daemon started at the same time gives up at once.
    let listeners = Listeners::bind(&config).await?;
    let idle_timeout = Duration::from_secs(config.idle_timeout_secs);
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
    let state = AppState {
        engine: engine.clone(),
        watchers: Arc::new(Watchers::default()),
        leases: Arc::new(Mutex::new(Leases::new(Instant::now()))),
        on_demand,
        request_lease: if idle_timeout.is_zero() {
            REQUEST_LEASE
        } else {
            REQUEST_LEASE.min(idle_timeout)
        },
    };
    let idle = Arc::new(tokio::sync::Notify::new());
    if on_demand {
        tokio::spawn(sweep(state.clone(), idle_timeout, idle.clone()));
    } else {
        for repo in engine.repos() {
            match engine.open_canonical(Path::new(&repo.root)) {
                Ok(w) => state.activate(w),
                Err(e) => tracing::warn!("cannot open {}: {e:#}", repo.root),
            }
        }
    }
    let app = router(state);
    let mode = if on_demand { ", on demand" } else { "" };
    eprintln!(
        "lodestone listening on {} (MCP at /mcp{mode})",
        listeners.describe()
    );
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let shutdown = move || {
        let mut stopped = stopped.clone();
        async move {
            let _ = stopped.wait_for(|s| *s).await;
        }
    };
    let mut servers = tokio::task::JoinSet::new();
    #[cfg(unix)]
    let (unix, socket) = listeners.unix;
    #[cfg(unix)]
    servers.spawn(
        axum::serve(unix, app.clone())
            .with_graceful_shutdown(shutdown())
            .into_future(),
    );
    if let Some((tcp, _)) = listeners.tcp {
        servers.spawn(
            axum::serve(tcp, app)
                .with_graceful_shutdown(shutdown())
                .into_future(),
        );
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = idle.notified() => eprintln!("lodestone: idle, exiting"),
    }
    let _ = stop.send(true);
    while let Some(r) = servers.join_next().await {
        r??;
    }
    #[cfg(unix)]
    drop(socket);
    Ok(())
}

/// Expires leases, stops watching worktrees no lease holds, and signals `idle` once no lease has
/// been live for `idle_timeout` (never, when it is zero) and no indexing pass is running.
async fn sweep(s: AppState, idle_timeout: Duration, idle: Arc<tokio::sync::Notify>) {
    loop {
        tokio::time::sleep(SWEEP).await;
        let now = Instant::now();
        let (live, idle_for) = {
            let mut l = s.leases();
            (l.sweep(now), l.idle_for(now))
        };
        for root in s.watchers.watched() {
            if !live.contains(&root) {
                s.watchers.unwatch(&s.engine, &root);
            }
        }
        if !idle_timeout.is_zero()
            && idle_for.is_some_and(|d| d >= idle_timeout)
            && !s.watchers.indexing()
        {
            idle.notify_one();
            return;
        }
    }
}
