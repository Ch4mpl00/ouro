// Idempotent state-database setup: connecting creates `mcp_state` if it is
// missing, applies its migrations and seeds the system tasks. The server
// does the same on boot; this does it without starting anything.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mcp_tools::cli::init();
    mcp_tools::cli::open_db().await?;
    println!("[setup:mcp] state database ready");
    Ok(())
}
