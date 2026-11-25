//! `lode mcp`: the MCP tools over stdio, for a harness that starts one per session. It reaches the
//! daemon (starting an on-demand one when none runs), leases the session's worktree while the
//! session lasts, and relays tool calls; a call without `repo` searches that worktree.
//!
//! A client on MCP `2026-07-28` or later that has roots is asked for them with each call, which then
//! searches the first (Antigravity starts the server in its plugin directory, and may serve several
//! workspaces from it); each root has a lease of its own. Otherwise the session's directory is the
//! one its hooks recorded (see [`marker`]) when Claude Code names the session
//! (`CLAUDE_CODE_SESSION_ID`): Claude Code in the desktop app starts MCP servers in the main checkout
//! of a worktree session. Failing both, it is this process's directory.

use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InputRequest,
    InputRequests, InputRequiredResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerConfig,
};
#[expect(deprecated, reason = "roots are how Antigravity names its workspace")]
use rmcp::model::{ListRootsRequest, ListRootsResult};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde::Deserialize;
use tokio::sync::watch;

use super::marker;
use crate::api::mcp::{server_info, tools};
use crate::api::types::{NOT_LOCAL, SessionRequest, ToolRequest};
use crate::client::{Client, START_WAIT, connect_or_start};
use crate::config::Config;
use crate::file_url;

const LEASE: Duration = Duration::from_secs(90);
const RENEW: Duration = Duration::from_secs(30);
/// How long a tool listing waits for the daemon to say whether it keeps code on this machine.
const LOCALITY_WAIT: Duration = Duration::from_secs(2);
/// Claude Code's id for the session that started this process.
const SESSION_ENV: &str = "CLAUDE_CODE_SESSION_ID";
/// A call's error when its client named no roots, and the call no `repo`.
const NO_ROOTS: &str = "the client named no workspace roots; pass `repo`";
/// The key of the roots request in a call's input requests and responses.
const ROOTS_INPUT: &str = "roots";

struct Daemon {
    config: Config,
    config_path: Option<PathBuf>,
    session: String,
    /// This process's directory.
    cwd: PathBuf,
    /// The harness session's marker, written by its hooks.
    marker: Option<PathBuf>,
    /// The roots calls have searched; the `i`th is leased as [`Self::root_id`]`(i)`.
    roots: Mutex<Vec<PathBuf>>,
    /// The directory the lease last named.
    leased: Mutex<PathBuf>,
    client: Mutex<Option<Arc<Client>>>,
    /// Whether the daemon serving the calls keeps code on this machine, as it said when leasing;
    /// `None` until one has said.
    local: watch::Sender<Option<bool>>,
    /// Whether the tools were last listed as local. Leases and calls made under that listing go
    /// to the daemon's local-only routes (see [`Client::tool_local`]), which a replacement daemon
    /// that does not keep code on this machine refuses, as does one too old to serve them.
    listed_local: AtomicBool,
}

impl Daemon {
    /// The session's directory: the one its hooks recorded, else this process's.
    fn dir(&self) -> PathBuf {
        self.marker
            .as_deref()
            .and_then(marker::read)
            .and_then(|m| m.cwd)
            .unwrap_or_else(|| self.cwd.clone())
    }

    fn root_id(&self, i: usize) -> String {
        format!("{}-root{i}", self.session)
    }

    /// The lease id for calls in the client's root `root`, which the renewals keep from now on.
    fn root_session(&self, root: &Path) -> String {
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        let i = roots.iter().position(|r| r == root).unwrap_or_else(|| {
            roots.push(root.to_path_buf());
            roots.len() - 1
        });
        self.root_id(i)
    }

    /// Every lease id this process holds.
    fn lease_ids(&self) -> Vec<String> {
        let roots = self.roots.lock().unwrap_or_else(|e| e.into_inner()).len();
        let mut ids = vec![self.session.clone()];
        ids.extend((0..roots).map(|i| self.root_id(i)));
        ids
    }

    fn listed_local(&self) -> bool {
        self.listed_local.load(Ordering::SeqCst)
    }

    /// Takes or renews the session's lease and every root's from `c`'s daemon.
    fn take_lease(&self, c: &Client, local_only: bool) -> anyhow::Result<()> {
        let dir = self.dir();
        *self.leased.lock().unwrap_or_else(|e| e.into_inner()) = dir.clone();
        self.lease(c, local_only, &self.session, &dir)?;
        let roots = self.roots.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (i, root) in roots.iter().enumerate() {
            self.lease(c, local_only, &self.root_id(i), root)?;
        }
        Ok(())
    }

    /// Takes or renews lease `id` on `dir` from `c`'s daemon (only one keeping code on this
    /// machine, when `local_only`), noting whether it keeps code on this machine.
    fn lease(&self, c: &Client, local_only: bool, id: &str, dir: &Path) -> anyhow::Result<()> {
        let req = SessionRequest {
            id: id.to_string(),
            cwd: dir.to_string_lossy().into_owned(),
            ttl_secs: LEASE.as_secs(),
        };
        let resp = if local_only {
            c.session_local(&req)?
        } else {
            c.session(&req)?
        };
        self.set_local(resp.local);
        Ok(())
    }

    fn set_local(&self, local: Option<bool>) {
        self.local
            .send_if_modified(|l| mem::replace(l, local) != local);
    }

    /// Re-leases when the hooks recorded another directory since the lease was last renewed, so a
    /// call right after a session's first prompt searches the session's worktree.
    fn follow_dir(&self, c: &Client, local_only: bool) -> anyhow::Result<()> {
        let moved = *self.leased.lock().unwrap_or_else(|e| e.into_inner()) != self.dir();
        if moved {
            self.take_lease(c, local_only)?;
        }
        Ok(())
    }

    fn current(&self) -> Option<Arc<Client>> {
        self.client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Reaches (or starts) the daemon and takes the lease. Holding the lock serializes starts.
    fn reconnect(&self, local_only: bool) -> anyhow::Result<Arc<Client>> {
        let mut slot = self.client.lock().unwrap_or_else(|e| e.into_inner());
        // Another daemon may answer now, with another config.
        self.set_local(None);
        let c = Arc::new(connect_or_start(
            &self.config,
            self.config_path.as_deref(),
            START_WAIT,
        )?);
        self.take_lease(&c, local_only)?;
        *slot = Some(c.clone());
        Ok(c)
    }

    /// Runs `f` against the daemon; if it fails (the daemon exited or crashed), starts the
    /// daemon again and retries once, leasing under the same `local_only`.
    fn with_client<T>(
        &self,
        local_only: bool,
        f: impl Fn(&Client) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let c = match self.current() {
            Some(c) => c,
            None => self.reconnect(local_only)?,
        };
        f(&c).or_else(|_| f(&*self.reconnect(local_only)?))
    }
}

#[derive(Clone)]
struct Relay {
    daemon: Arc<Daemon>,
}

impl ServerHandler for Relay {
    /// Tools can change: their descriptions follow the daemon's locality.
    fn get_info(&self) -> ServerConfig {
        let mut info = server_info();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        info
    }

    /// The search tools promise that code stays on this machine only once the daemon serving
    /// them has said so; until then (it may still be starting), they make no promise, and the
    /// harness is told when the list changes.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut local = self.daemon.local.subscribe();
        // A running daemon answers the first lease within moments of startup.
        let _ = tokio::time::timeout(LOCALITY_WAIT, local.wait_for(Option::is_some)).await;
        let local = *local.borrow() == Some(true);
        self.daemon.listed_local.store(local, Ordering::SeqCst);
        Ok(ListToolsResult::with_all_items(tools(local)))
    }

    /// Asks the client for its roots first, when [`offers_roots`]: the client calls again with
    /// them. Each call asks, since a client may serve sessions in several workspaces. A client
    /// that names no roots must name the call's `repo`.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let d = self.daemon.clone();
        let roots = request
            .input_responses
            .as_ref()
            .and_then(|r| r.get(ROOTS_INPUT));
        if roots.is_none() && offers_roots(&context) {
            return Ok(ask_roots().into());
        }
        let root = roots.and_then(first_root);
        let names_repo = request
            .arguments
            .as_ref()
            .is_some_and(|a| a.get("repo").is_some_and(|r| r.is_string()));
        if roots.is_some() && root.is_none() && !names_repo {
            return Ok(CallToolResult::error(vec![ContentBlock::text(NO_ROOTS)]).into());
        }
        // Fixed when the call is accepted: a re-list while it waits does not loosen it.
        let local_only = d.listed_local();
        let req = ToolRequest {
            name: request.name.to_string(),
            arguments: request.arguments.unwrap_or_default(),
            session: None,
            cwd: None,
        };
        let out = tokio::task::spawn_blocking(move || {
            d.with_client(local_only, |c| {
                let (session, dir) = match &root {
                    Some(root) => {
                        let id = d.root_session(root);
                        d.lease(c, local_only, &id, root)?;
                        (id, root.clone())
                    }
                    None => {
                        d.follow_dir(c, local_only)?;
                        (d.session.clone(), d.dir())
                    }
                };
                let req = ToolRequest {
                    session: Some(session),
                    cwd: Some(dir.to_string_lossy().into_owned()),
                    ..req.clone()
                };
                if local_only {
                    c.tool_local(&req)
                } else {
                    c.tool(&req)
                }
            })
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| format!("{e:#}")));
        if let Ok(Err(msg)) = &out
            && msg.starts_with(NOT_LOCAL)
        {
            // Lists the tools again, without the promise.
            self.daemon.set_local(Some(false));
        }
        let result = match out {
            Ok(Ok(text)) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Ok(Err(msg)) | Err(msg) => CallToolResult::error(vec![ContentBlock::text(msg)]),
        };
        Ok(result.into())
    }
}

/// Whether the client has roots and can send them with a call (MCP `2026-07-28` or later).
fn offers_roots(context: &RequestContext<RoleServer>) -> bool {
    context
        .client_capabilities()
        .is_some_and(|c| c.roots.is_some())
        && context
            .protocol_version()
            .is_some_and(|v| v >= ProtocolVersion::V_2026_07_28)
}

/// A call's answer asking the client for its roots.
#[expect(deprecated, reason = "roots are how Antigravity names its workspace")]
fn ask_roots() -> InputRequiredResult {
    InputRequiredResult::from_input_requests(InputRequests::from([(
        ROOTS_INPUT.to_string(),
        InputRequest::ListRoots(ListRootsRequest::default()),
    )]))
}

/// The path of the first root in a client's `roots/list` result.
#[expect(deprecated, reason = "roots are how Antigravity names its workspace")]
fn first_root(result: &serde_json::Value) -> Option<PathBuf> {
    let result = ListRootsResult::deserialize(result).ok()?;
    let root = result.roots.first()?;
    Some(file_url::local_path(&root.uri))
}

/// Serves the relay on stdin/stdout until the harness closes it, then releases the leases.
pub async fn run(config: Config, config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let cwd = std::env::current_dir()?;
    let marker = std::env::var(SESSION_ENV)
        .ok()
        .and_then(|id| marker::path(&config.data_dir(), &id));
    let daemon = Arc::new(Daemon {
        config,
        config_path,
        session: format!("mcp-{}-{nanos}", std::process::id()),
        leased: Mutex::new(cwd.clone()),
        cwd,
        marker,
        roots: Mutex::new(Vec::new()),
        client: Mutex::new(None),
        local: watch::Sender::new(None),
        listed_local: AtomicBool::new(false),
    });
    let mut locality = daemon.local.subscribe();
    // Reach or start the daemon now, so the model loads while the harness starts up; tool calls
    // wait for it if it is not ready.
    let warm = daemon.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = warm.reconnect(warm.listed_local()) {
            tracing::warn!("{e:#}");
        }
    });
    let renewer = daemon.clone();
    let renew = tokio::spawn(async move {
        loop {
            tokio::time::sleep(RENEW).await;
            let d = renewer.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let local_only = d.listed_local();
                d.with_client(local_only, |c| d.take_lease(c, local_only))
            })
            .await;
        }
    });
    let service = Relay {
        daemon: daemon.clone(),
    }
    .serve(rmcp::transport::stdio())
    .await?;
    let peer = service.peer().clone();
    let notify = tokio::spawn(async move {
        while locality.changed().await.is_ok() {
            if let Err(e) = peer.notify_tool_list_changed().await {
                tracing::warn!("tools/list_changed: {e}");
            }
        }
    });
    let _ = service.waiting().await;
    renew.abort();
    notify.abort();
    tokio::task::spawn_blocking(move || {
        if let Some(c) = daemon.current() {
            for id in daemon.lease_ids() {
                let _ = c.release(&id);
            }
        }
    })
    .await?;
    Ok(())
}
