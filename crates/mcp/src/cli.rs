// Shared plumbing for the CLI entry points in src/bin: env files, logging,
// `--flag value` arguments, and the default locations they all agree on.

use std::path::PathBuf;

pub const DEFAULT_DB_PATH: &str = "packages/mcp/data/tokens.db";
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

pub fn db_path() -> PathBuf {
    std::env::var("MCP_DB_PATH")
        .ok()
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_DB_PATH), PathBuf::from)
}

pub fn open_db() -> anyhow::Result<crate::db::Db> {
    let path = db_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::db::Db::open(&path)
}

pub fn prompt(message: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{message}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}
