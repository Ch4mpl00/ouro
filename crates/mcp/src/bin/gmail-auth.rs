// One-time Gmail OAuth: consent URL → paste the `code` → tokens stored.

use mcp_tools::{cli, gmail::GmailModule};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let gmail = GmailModule::new(cli::open_db()?, reqwest::Client::new());
    println!("\n1) Open this URL in a browser and grant access:\n\n{}", gmail.auth_url()?);
    println!("\n2) After consent, Google will redirect to your GOOGLE_REDIRECT_URI with a `code` query param.");
    println!("   Copy that `code` value and paste it below.\n");
    let code = cli::prompt("code: ")?;
    anyhow::ensure!(!code.is_empty(), "No code provided");
    let account = gmail.exchange_code_and_persist(&code).await?;
    println!("\nGmail authorized for {account}");
    Ok(())
}
