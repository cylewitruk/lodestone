//! `lode mcp`: the MCP tools over stdio, for a harness that starts one per session. It reaches the
//! daemon (starting an on-demand one when none runs), leases the session's worktree while the
//! session lasts, and relays tool calls; a call without `repo` searches that worktree.
//!
//! The session's directory is the one its hooks recorded (see [`marker`]) when Claude Code names the
//! session (`CLAUDE_CODE_SESSION_ID`), otherwise this process's: Claude Code in the desktop app
//! starts MCP servers in the main checkout of a worktree session.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};

use super::marker;
use crate::api::mcp::{server_info, tools};
use crate::api::types::{SessionRequest, ToolRequest};
use crate::client::{Client, START_WAIT, connect_or_start};
use crate::config::Config;

const LEASE: Duration = Duration::from_secs(90);
const RENEW: Duration = Duration::from_secs(30);
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

    /// Re-leases when the hooks recorded another directory since the lease was last renewed, so a
    /// call right after a session's first prompt searches the session's worktree.
    fn follow_dir(&self, c: &Client) -> anyhow::Result<()> {
        let moved = *self.leased.lock().unwrap_or_else(|e| e.into_inner()) != self.dir();
        if moved {
            c.session(&self.lease())?;
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
    fn reconnect(&self) -> anyhow::Result<Arc<Client>> {
        let mut slot = self.client.lock().unwrap_or_else(|e| e.into_inner());
        let c = Arc::new(connect_or_start(
            &self.config,
            self.config_path.as_deref(),
            START_WAIT,
        )?);
        c.session(&self.lease())?;
        *slot = Some(c.clone());
        Ok(c)
    }

    /// Runs `f` against the daemon; if it fails (the daemon exited or crashed), starts the
    /// daemon again and retries once.
    fn with_client<T>(&self, f: impl Fn(&Client) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let c = match self.current() {
            Some(c) => c,
            None => self.reconnect()?,
        };
        f(&c).or_else(|_| f(&*self.reconnect()?))
    }
}

#[derive(Clone)]
struct Relay {
    daemon: Arc<Daemon>,
}

impl ServerHandler for Relay {
    fn get_info(&self) -> ServerConfig {
        server_info()
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
        let d = self.daemon.clone();
        let req = ToolRequest {
            name: request.name.to_string(),
            arguments: request.arguments.unwrap_or_default(),
            session: Some(d.session.clone()),
        };
        let out = tokio::task::spawn_blocking(move || {
            d.with_client(|c| {
                d.follow_dir(c)?;
                c.tool(&req)
            })
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| format!("{e:#}")));
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
    });
    // Reach or start the daemon now, so the model loads while the harness starts up; tool calls
    // wait for it if it is not ready.
    let warm = daemon.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = warm.reconnect() {
            tracing::warn!("{e:#}");
        }
    });
    let renewer = daemon.clone();
    let renew = tokio::spawn(async move {
        loop {
            tokio::time::sleep(RENEW).await;
            let d = renewer.clone();
            let _ = tokio::task::spawn_blocking(move || {
                d.with_client(|c| c.session(&d.lease()).map(|_| ()))
            })
            .await;
        }
    });
    let service = Relay {
        daemon: daemon.clone(),
    }
    .serve(rmcp::transport::stdio())
    .await?;
    let _ = service.waiting().await;
    renew.abort();
    tokio::task::spawn_blocking(move || {
        if let Some(c) = daemon.current() {
            let _ = c.release(&daemon.session);
        }
    })
    .await?;
    Ok(())
}
