// Entry point of the codex service (`codex-service`).
use codex_service::server::{ServerOptions, router};

fn number(name: &str, fallback: usize) -> anyhow::Result<usize> {
    match std::env::var(name).ok().filter(|v| !v.is_empty()) {
        None => Ok(fallback),
        Some(raw) => raw.parse().map_err(|_| anyhow::anyhow!("{name} must be a number")),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port = number("CODEX_SERVICE_PORT", 3010)?;
    let max_body_bytes = number("CODEX_SERVICE_MAX_BODY_BYTES", 10 * 1024 * 1024)?;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port as u16)).await?;
    println!("[codex] service listening on :{port}");
    axum::serve(listener, router(ServerOptions { max_body_bytes }))
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
            }
        })
        .await?;
    Ok(())
}
