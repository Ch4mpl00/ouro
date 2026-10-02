// Debug helper: list matching mail. --account, --query (is:unread), --limit (10).

use mcp_tools::{cli, gmail::GmailModule};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let gmail = GmailModule::new(cli::open_db().await?, reqwest::Client::new());
    let account = match cli::arg("account") {
        Some(a) => a,
        None => gmail
            .resolve_account_key()
            .await?
            .ok_or_else(|| anyhow::anyhow!("No Gmail account in DB. Run `pnpm gmail:auth` first."))?,
    };
    let query = cli::arg("query").unwrap_or_else(|| "is:unread".into());
    let limit: u32 = cli::arg("limit").map_or(Ok(10), |l| l.parse())?;
    let page = gmail.list_messages(&account, &query, limit, None).await?;
    println!("\n{account} — query=`{query}` — {} match(es)\n", page.messages.len());
    for m in page.messages {
        println!("• {}", m.subject.as_deref().unwrap_or("(no subject)"));
        println!("  from: {}", m.from.as_deref().unwrap_or("?"));
        if let Some(date) = &m.date {
            println!("  date: {date}");
        }
        if !m.snippet.is_empty() {
            println!("  {}", m.snippet);
        }
        println!();
    }
    Ok(())
}
