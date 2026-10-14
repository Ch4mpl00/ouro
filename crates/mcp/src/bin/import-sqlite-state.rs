// One-shot move of the TS-era sqlite state (`tokens.db`) into the Postgres
// `mcp_state` database. Refuses to run into a database that already holds
// data, so it is safe to re-run by mistake.
//
//   import-sqlite-state --sqlite /path/to/tokens.db
//   docker compose run --rm -v agent-helper_mcp-data:/legacy:ro mcp \
//     import-sqlite-state --sqlite /legacy/tokens.db

use mcp_tools::{cli, db};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let path = cli::legacy_sqlite_path();
    anyhow::ensure!(path.exists(), "no sqlite file at {}", path.display());
    let state = cli::open_db().await?;
    let report = db::import_sqlite(&state, &path).await?;
    println!("[import-sqlite-state] {} → mcp_state: {}", path.display(), serde_json::to_string(&report)?);
    Ok(())
}
