// Print a statement as JSON. --account (0 = default UAH), --days (7, max 31).

use mcp_tools::{cli, monobank::Monobank};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let account = cli::arg("account").unwrap_or_else(|| "0".into());
    let days: i64 = cli::arg("days").map_or(Ok(7), |d| d.parse())?;
    anyhow::ensure!((1..=31).contains(&days), "--days must be between 1 and 31 (got {days})");
    let statement = Monobank::new(reqwest::Client::new()).recent(&account, days).await?;
    println!("{}", serde_json::to_string_pretty(&statement)?);
    Ok(())
}
