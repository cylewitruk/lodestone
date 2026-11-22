//! `lode mcp`: the MCP tools over stdio, for a harness that starts one per session. It reaches the
//! daemon (starting an on-demand one when none runs), leases the session's worktree while the
//! session lasts, and relays tool calls; a call without `repo` searches that worktree.
//!
//! The session's directory is the one its hooks recorded (see [`marker`]) when Claude Code names the
//! session (`CLAUDE_CODE_SESSION_ID`), otherwise this process's: Claude Code in the desktop app
//! starts MCP servers in the main checkout of a worktree session.

use std::mem;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use tokio::sync::watch;

use super::marker;
use crate::api::mcp::{server_info, tools};
use crate::api::types::{NOT_LOCAL, SessionRequest, ToolRequest};
use crate::client::{Client, START_WAIT, connect_or_start};
use crate::config::Config;

const LEASE: Duration = Duration::from_secs(90);
const RENEW: Duration = Duration::from_secs(30);
/// How long a tool listing waits for the daemon to say whether it keeps code on this machine.
const LOCALITY_WAIT: Duration = Duration::from_secs(2);
/// Claude Code's id for the session that started this process.
const SESSION_ENV: &str = "CLAUDE_CODE_SESSION_ID";

struct Daemon {
    config: Config,
    config_path: Option<PathBuf>,
    session: String,
    /// This process's directory.
    cwd: PathBuf,
    /// The harness session's marker, written by its hooks.
    marker: Option<PathBuf>,
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

    fn lease(&self) -> SessionRequest {
        let dir = self.dir();
        let cwd = dir.to_string_lossy().into_owned();
        *self.leased.lock().unwrap_or_else(|e| e.into_inner()) = dir;
        SessionRequest {
            id: self.session.clone(),
            cwd,
            ttl_secs: LEASE.as_secs(),
        }
    }

    fn listed_local(&self) -> bool {
        self.listed_local.load(Ordering::SeqCst)
    }

    /// Takes or renews the lease from `c`'s daemon (only one keeping code on this machine, when
    /// `local_only`), noting whether it keeps code on this machine.
    fn take_lease(&self, c: &Client, local_only: bool) -> anyhow::Result<()> {
        let req = self.lease();
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

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let d = self.daemon.clone();
        // Fixed when the call is accepted: a re-list while it waits does not loosen it.
        let local_only = d.listed_local();
        let req = ToolRequest {
            name: request.name.to_string(),
            arguments: request.arguments.unwrap_or_default(),
            session: Some(d.session.clone()),
            cwd: None,
        };
        let out = tokio::task::spawn_blocking(move || {
            d.with_client(local_only, |c| {
                d.follow_dir(c, local_only)?;
                let req = ToolRequest {
                    cwd: Some(d.dir().to_string_lossy().into_owned()),
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

/// Serves the relay on stdin/stdout until the harness closes it, then releases the lease.
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
            let _ = c.release(&daemon.session);
        }
    })
    .await?;
    Ok(())
}
