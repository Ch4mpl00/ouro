// Dump the most recent news_items into the eval corpus fixture (no
// embeddings — each config re-embeds from text). Re-ordered by id so
// re-runs diff cleanly; body-less rows dropped.
//   eval-snapshot [--limit 2000] [--out crates/mcp/eval/fixtures/corpus.jsonl]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use mcp_tools::time::iso;
use mcp_tools::{cli, pg};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let out = cli::arg("out").map_or_else(|| Path::new(cli::EVAL_DIR).join("fixtures/corpus.jsonl"), PathBuf::from);
    let limit: i64 = cli::arg("limit").map_or(Ok(2000), |l| l.parse())?;
    anyhow::ensure!(limit > 0, "--limit must be a positive number, got {limit}");

    let pool = pg::connect(&pg::database_url()?)?;
    pg::migrate(&pool).await?;
    let rows = pool
        .get()
        .await?
        .query(
            "SELECT id, source, external_id, title, url, body, metadata, posted_at FROM (
               SELECT *, COALESCE(posted_at, fetched_at) AS sort_at FROM news_items
                WHERE length(body) > 0 ORDER BY sort_at DESC LIMIT $1
             ) sub ORDER BY id ASC",
            &[&limit],
        )
        .await?;
    let mut by_source: BTreeMap<String, usize> = BTreeMap::new();
    let mut lines = String::new();
    for r in &rows {
        let source: String = r.get("source");
        *by_source.entry(source.clone()).or_default() += 1;
        let line = json!({
            "id": r.get::<_, i64>("id"),
            "source": source,
            "externalId": r.get::<_, String>("external_id"),
            "title": r.get::<_, Option<String>>("title"),
            "url": r.get::<_, Option<String>>("url"),
            "body": r.get::<_, String>("body"),
            "metadata": r.get::<_, Option<Value>>("metadata").unwrap_or_else(|| json!({})),
            "postedAt": r.get::<_, Option<DateTime<Utc>>>("posted_at").map(iso),
        });
        lines.push_str(&line.to_string());
        lines.push('\n');
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&out, lines)?;
    println!("[eval-snapshot] wrote {} rows (limit={limit}) → {}", rows.len(), out.display());
    println!("[eval-snapshot] by source: {by_source:?}");
    Ok(())
}
