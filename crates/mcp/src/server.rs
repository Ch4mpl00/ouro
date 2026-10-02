// The MCP endpoint: the tool-serving handler every domain file adds its
// `#[tool_router]` block to, and the two transports it is served over.
//
// Sections:
//   1. handler   — `McpTools` + its injected `Deps`
//   2. results   — the JSON-text result shape and error helpers tools share
//   3. sessions  — "newest wins" policy for the single-session instance
//   4. transport — stdio and Streamable HTTP

use std::sync::Arc;

use futures::Stream;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{CallToolResult, ClientJsonRpcMessage, ContentBlock, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::local::{LocalSessionManager, LocalSessionManagerError};
use rmcp::transport::streamable_http_server::session::{EventStore, ServerSseMessage};
use rmcp::transport::streamable_http_server::{RestoreOutcome, SessionId, SessionManager};
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, ServiceExt, tool_handler};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::scheduler::Scheduler;
use crate::settings::Settings;
use crate::signals::Signals;
use crate::telegram::TelegramConfig;

// ── 1. handler ───────────────────────────────────────────────────────────────

// Everything a tool handler may touch, built once in `main` and shared by
// every session. A handler's reach is exactly this struct — no globals.
pub struct Deps {
    pub settings: Settings,
    pub signals: Signals,
    pub scheduler: Scheduler,
    pub telegram: TelegramConfig,
}

// One per MCP session. Cheap to build: the deps are shared and the router is
// a clone of the one composed from the selected toolsets.
#[derive(Clone)]
pub struct McpTools {
    pub(crate) deps: Arc<Deps>,
    tool_router: ToolRouter<Self>,
}

impl McpTools {
    pub fn new(deps: Arc<Deps>, tool_router: ToolRouter<Self>) -> Self {
        Self { deps, tool_router }
    }
}

#[tool_handler(router = self.tool_router.clone(), name = "mcp-tools", version = "0.1.0")]
impl ServerHandler for McpTools {}

// ── 2. results ───────────────────────────────────────────────────────────────

// A single text block of pretty JSON — what every tool has always returned,
// and what clients without structured-content support can still read.
pub fn json_result(value: &impl Serialize) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(value).map_err(internal)?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

pub fn internal(err: impl std::fmt::Display) -> ErrorData {
    tracing::error!(%err, "tool failed");
    ErrorData::internal_error(err.to_string(), None)
}

pub fn invalid_params(message: &'static str) -> ErrorData {
    ErrorData::invalid_params(message, None)
}

// ── 3. sessions ──────────────────────────────────────────────────────────────

// The full instance serves exactly one client — the supervisor — and a fresh
// `initialize` means the previous one is gone (it restarted after an unclean
// death and never closed its session). So the newest session evicts the rest
// instead of being refused: refusing is what crash-looped the TS server in
// production (2026-06-15, 2026-08-23 — 78 restarts).
//
// A restricted instance (MCP_TOOLSETS set) has no signal delivery to race on
// and must hold several sessions at once — tunnel-client keeps a probe session
// of its own, so a ChatGPT client is always the second connection.
pub struct SessionPolicy {
    inner: LocalSessionManager,
    newest_wins: bool,
}

impl SessionPolicy {
    pub fn new(newest_wins: bool) -> Self {
        Self { inner: LocalSessionManager::default(), newest_wins }
    }
}

impl SessionManager for SessionPolicy {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        if self.newest_wins {
            let stale: Vec<SessionId> = self.inner.sessions.read().await.keys().cloned().collect();
            for id in stale {
                tracing::info!(session = %id, "evicting previous session: newest wins");
                if let Err(err) = self.inner.close_session(&id).await {
                    tracing::warn!(session = %id, %err, "evicting stale session failed");
                }
            }
        }
        self.inner.create_session().await
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        self.inner.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.inner.has_session(id).await
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        self.inner.close_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.create_stream(id, message).await
    }

    async fn accept_message(&self, id: &SessionId, message: ClientJsonRpcMessage) -> Result<(), Self::Error> {
        self.inner.accept_message(id, message).await
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.create_standalone_stream(id).await
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.resume(id, last_event_id).await
    }

    async fn restore_session(&self, id: SessionId) -> Result<RestoreOutcome<Self::Transport>, Self::Error> {
        self.inner.restore_session(id).await
    }

    fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.inner.event_store()
    }
}

// ── 4. transport ─────────────────────────────────────────────────────────────

pub async fn serve_stdio(tools: McpTools, cancel: CancellationToken) -> anyhow::Result<()> {
    let running = tools.serve_with_ct(rmcp::transport::stdio(), cancel).await?;
    running.waiting().await?;
    Ok(())
}

pub struct HttpOptions {
    pub port: u16,
    // Hosts (`host` or `host:port`) accepted in the Host header. rmcp only
    // accepts loopback by default, which would reject the compose service
    // names (`mcp:3000`, `mcp-tunnel:3001`) the agent and tunnel dial.
    pub allowed_hosts: Vec<String>,
    pub multi_session: bool,
}

pub async fn serve_http(
    make_tools: impl Fn() -> McpTools + Send + Sync + 'static,
    options: HttpOptions,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let mut config = StreamableHttpServerConfig::default().with_allowed_hosts(options.allowed_hosts);
    config.cancellation_token = cancel.child_token();
    let service = StreamableHttpService::new(
        move || Ok(make_tools()),
        Arc::new(SessionPolicy::new(!options.multi_session)),
        config,
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", options.port)).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        multi_session = options.multi_session,
        "mcp http listening"
    );
    axum::serve(listener, app).with_graceful_shutdown(cancel.cancelled_owned()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn newest_session_evicts_the_previous_one() {
        let policy = SessionPolicy::new(true);
        let (first, _t1) = policy.create_session().await.unwrap();
        let (second, _t2) = policy.create_session().await.unwrap();
        assert!(!policy.has_session(&first).await.unwrap());
        assert!(policy.has_session(&second).await.unwrap());
    }

    #[tokio::test]
    async fn multi_session_keeps_every_session() {
        let policy = SessionPolicy::new(false);
        let (first, _t1) = policy.create_session().await.unwrap();
        let (second, _t2) = policy.create_session().await.unwrap();
        assert!(policy.has_session(&first).await.unwrap());
        assert!(policy.has_session(&second).await.unwrap());
    }
}
