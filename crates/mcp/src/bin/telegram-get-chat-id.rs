// Discover chat ids: message the bot first, then run this.

use std::collections::BTreeMap;

use mcp_tools::cli;
use mcp_tools::telegram::{BotApi, TelegramConfig, chat_label};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let bot = BotApi::new(reqwest::Client::new(), TelegramConfig::from_env().bot_token);
    let updates = bot.get_updates(None, None).await?;
    if updates.is_empty() {
        println!("No updates yet. Open Telegram, start a chat with your bot, send any message, then re-run.");
        return Ok(());
    }
    let mut seen = BTreeMap::new();
    for u in &updates {
        let chat = u.message.as_ref().or(u.edited_message.as_ref()).or(u.channel_post.as_ref()).map(|m| m.chat.clone());
        if let Some(chat) = chat {
            seen.entry(chat.id).or_insert(chat);
        }
    }
    println!("Found {} distinct chat(s):\n", seen.len());
    for chat in seen.values() {
        println!("  chat_id={}  type={}  {}", chat.id, chat.kind, chat_label(chat));
    }
    println!("\nSet TELEGRAM_DEFAULT_CHAT_ID in .env to the chat_id you want notifications routed to.");
    Ok(())
}
