// Composition root of the Rust MCP server — the only place that knows the
// whole graph. Builds every long-lived dependency once, threads it into the
// tool handler and the pollers, and owns shutdown.
//
// Port status: signals, dreaming (list_signals) and scheduler are in Rust;
// the other toolsets are refused at boot until their domain moves over (see
// toolsets.rs). Plan and order: .claude/tasks/rust-rewrite.md.

mod db;
mod scheduler;
mod server;
mod settings;
mod signals;
mod telegram;
mod toolsets;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::db::Db;
use crate::scheduler::Scheduler;
use crate::server::{Deps, HttpOptions, McpTools};
use crate::settings::Settings;
use crate::signals::Signals;
use crate::telegram::TelegramConfig;

const DEFAULT_DB_PATH: &str = "packages/mcp/data/tokens.db";
const DEFAULT_ALLOWED_HOSTS: &str = "localhost,127.0.0.1,::1";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs go to stderr: on the stdio transport stdout is the MCP channel.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();

    let selection = toolsets::parse_toolsets(env("MCP_TOOLSETS").as_deref())?;
    let router = toolsets::compose_router(&selection.names)?;
    if selection.restricted {
        let names: Vec<&str> = selection.names.iter().map(|t| t.name()).collect();
        tracing::info!(toolsets = names.join(","), "restricted tool surface");
    }

    let db_path = env("MCP_DB_PATH").map_or_else(|| PathBuf::from(DEFAULT_DB_PATH), PathBuf::from);
    let db = Db::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    let settings = Settings::new(db.clone());
    let signals = Signals::new(db.clone());
    let scheduler = Scheduler::new(db.clone(), settings.clone());
    let deps = Arc::new(Deps {
        settings,
        signals: signals.clone(),
        scheduler: scheduler.clone(),
        telegram: TelegramConfig::from_env(),
    });
    let make_tools = move || McpTools::new(deps.clone(), router.clone());

    let cancel = CancellationToken::new();
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            cancel.cancel();
        }
    });

    // MCP_NO_POLLERS: a tools-only instance (eval harness, the ChatGPT tunnel).
    // The Telegram getUpdates poll is exclusive per bot, so a second poller
    // would 409 the droplet.
    let mut pollers = tokio::task::JoinSet::new();
    if env("MCP_NO_POLLERS").as_deref() == Some("1") {
        tracing::info!("MCP_NO_POLLERS=1 — tools only, pollers disabled");
    } else {
        pollers.spawn(scheduler::run_poller(scheduler, signals, cancel.clone()));
    }

    let transport = env("MCP_TRANSPORT").unwrap_or_else(|| "stdio".into()).to_lowercase();
    let served = if transport == "http" {
        let port = env("MCP_PORT").map_or(Ok(3000), |p| p.parse()).context("MCP_PORT must be a port number")?;
        let allowed_hosts = env("MCP_ALLOWED_HOSTS")
            .unwrap_or_else(|| DEFAULT_ALLOWED_HOSTS.into())
            .split(',')
            .map(|h| h.trim().to_owned())
            .filter(|h| !h.is_empty())
            .collect();
        let options = HttpOptions { port, allowed_hosts, multi_session: selection.restricted };
        server::serve_http(make_tools, options, cancel.clone()).await
    } else {
        server::serve_stdio(make_tools(), cancel.clone()).await
    };

    // The transport ending (stdin closed, or a shutdown signal) ends the
    // process; let the pollers observe the cancel and finish their tick.
    cancel.cancel();
    while pollers.join_next().await.is_some() {}
    served
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(term) => term,
            Err(err) => {
                tracing::warn!(%err, "SIGTERM handler unavailable, listening for Ctrl-C only");
                let _ = ctrl_c.await;
                return;
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}
