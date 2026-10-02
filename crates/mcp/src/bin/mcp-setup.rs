// Idempotent sqlite setup: opening the DB applies the schema, the additive
// migrations and the system-task seed. Safe on every container boot.

fn main() -> anyhow::Result<()> {
    mcp_tools::cli::init();
    mcp_tools::cli::open_db()?;
    println!("[setup:mcp] schema applied to {}", mcp_tools::cli::db_path().display());
    Ok(())
}
