// One-shot copy of the legacy sqlite `channel_posts` (in the TS-era
// tokens.db, --sqlite <path>) into the news store, embedding inline.
// Idempotent (ON CONFLICT DO NOTHING).

use std::sync::Arc;

use mcp_tools::embeddings::OpenAiEmbedder;
use mcp_tools::news::{NewsItem, NewsRepository};
use mcp_tools::time::parse_js_date;
use mcp_tools::{cli, pg};
use serde_json::{Map, Value};

const BATCH: usize = 100;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let rows: Vec<NewsItem> = {
        let conn = rusqlite::Connection::open_with_flags(
            cli::legacy_sqlite_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut stmt = conn.prepare(
            "SELECT chat_id, chat_title, chat_username, tg_message_id, posted_at, text, views, forwards
               FROM channel_posts ORDER BY posted_at ASC",
        )?;
        let mapped = stmt.query_map([], |r| {
            let (chat_id, title, username): (String, Option<String>, Option<String>) =
                (r.get(0)?, r.get(1)?, r.get(2)?);
            let (msg_id, posted_at, text): (i64, String, String) = (r.get(3)?, r.get(4)?, r.get(5)?);
            let (views, forwards): (Option<i64>, Option<i64>) = (r.get(6)?, r.get(7)?);
            let mut metadata = Map::new();
            metadata.insert("chat_id".into(), chat_id.clone().into());
            metadata.insert("chat_title".into(), title.into());
            metadata.insert("chat_username".into(), username.clone().into());
            metadata.insert("tg_message_id".into(), msg_id.into());
            metadata.insert("views".into(), views.map_or(Value::Null, Value::from));
            metadata.insert("forwards".into(), forwards.map_or(Value::Null, Value::from));
            Ok(NewsItem {
                source: "channel".into(),
                external_id: format!("{chat_id}:{msg_id}"),
                title: None,
                url: username.map(|u| format!("https://t.me/{u}/{msg_id}")),
                body: text,
                metadata,
                posted_at: parse_js_date(&posted_at),
            })
        })?;
        mapped.collect::<Result<_, _>>()?
    };
    println!("[migrate] sqlite has {} rows to migrate", rows.len());

    let pool = pg::connect(&pg::database_url()?)?;
    pg::migrate(&pool).await?;
    let news = NewsRepository::new(pool, Arc::new(OpenAiEmbedder::from_env(reqwest::Client::new())?));
    let (mut saved, mut embedded, mut failed) = (0, 0, 0);
    for (i, batch) in rows.chunks(BATCH).enumerate() {
        let r = news.save(batch).await?;
        saved += r.saved;
        embedded += r.embedded;
        failed += r.failed;
        println!(
            "[migrate] batch {}/{} (saved={}, embedded={embedded}, failed={failed})",
            i * BATCH + batch.len(),
            rows.len(),
            r.saved
        );
    }
    println!("[migrate] done: saved={saved}, embedded={embedded}, embed-failed={failed}");
    Ok(())
}
