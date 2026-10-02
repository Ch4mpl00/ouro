// Shared plumbing for the CLI entry points in src/bin: env files, logging,
// `--flag value` arguments, and the default locations they all agree on.

use std::path::PathBuf;

// The pre-Postgres sqlite file, read only by the one-shot importers.
pub const LEGACY_SQLITE_PATH: &str = "packages/mcp/data/tokens.db";
pub const EVAL_DIR: &str = "crates/mcp/eval";

// `.env` like the server, plus `.env.mcp` — the CLIs run on a dev machine
// where the container env isn't injected.
pub fn init() {
    // The working directory's files only — dotenvy::dotenv() would also walk
    // up parent directories.
    let _ = dotenvy::from_path(".env");
    let _ = dotenvy::from_path(".env.mcp");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

pub fn arg(name: &str) -> Option<String> {
    let flag = format!("--{name}");
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| *a == flag).and_then(|i| args.get(i + 1)).cloned()
}

pub fn legacy_sqlite_path() -> PathBuf {
    arg("sqlite").map_or_else(|| PathBuf::from(LEGACY_SQLITE_PATH), PathBuf::from)
}

// The state database (`mcp_state`, next to the news store) — created and
// migrated on open, like the server does.
pub async fn open_db() -> anyhow::Result<crate::db::Db> {
    crate::db::Db::connect(&crate::pg::database_url()?).await
}

pub fn prompt(message: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{message}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}
