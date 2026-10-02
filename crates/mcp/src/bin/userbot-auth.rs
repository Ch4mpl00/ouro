// One-time MTProto login for the userbot: phone → code → (2FA) → the session
// saved in integration_account, in the format the server reads.

use mcp_tools::cli;
use mcp_tools::userbot::{Prompts, Userbot, login};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let userbot = Userbot::new(cli::open_db().await?);
    println!("Starting Telegram userbot login (MTProto)...");
    let (account, username) = login(
        &userbot,
        Prompts {
            phone: &|| cli::prompt("Phone number (e.g. +380501234567): "),
            code: &|| cli::prompt("Code from Telegram: "),
            password: &|hint| cli::prompt(&format!("2FA password (hint: {}): ", hint.unwrap_or("none"))),
        },
    )
    .await?;
    println!("\n✓ Saved session for account {account}{}", username.map(|u| format!(" (@{u})")).unwrap_or_default());
    Ok(())
}
