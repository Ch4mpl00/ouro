// Re-attempts embeddings left NULL by a failed inline embed, in every
// embedded store. Safe to re-run.
//
//   docker compose exec mcp embed-backfill

use std::sync::Arc;

use mcp_tools::embeddings::{OpenAiEmbedder, SharedEmbedder};
use mcp_tools::knowledge::KnowledgeRepository;
use mcp_tools::memory::{MemoryService, PgMemoryStore};
use mcp_tools::news::{EmbedResult, NewsRepository};
use mcp_tools::{cli, pg};

const BATCH: i64 = 100;

// One batch at a time until empty — or until a whole batch fails, which
// points at auth/API trouble that retrying won't fix.
async fn drain<F, Fut>(label: &str, mut next: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<EmbedResult>>,
{
    let (mut embedded, mut failed) = (0, 0);
    loop {
        let r = next().await?;
        if r.embedded == 0 && r.failed == 0 {
            break;
        }
        embedded += r.embedded;
        failed += r.failed;
        println!(
            "[embed-backfill:{label}] batch: embedded={}, failed={} (running totals: {embedded}/{failed})",
            r.embedded, r.failed
        );
        if r.failed > 0 {
            eprintln!("[embed-backfill:{label}] giving up after batch failure");
            break;
        }
    }
    println!("[embed-backfill:{label}] done: embedded={embedded}, failed={failed}");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let pool = pg::connect(&pg::database_url()?)?;
    pg::migrate(&pool).await?;
    let embedder: SharedEmbedder = Arc::new(OpenAiEmbedder::from_env(reqwest::Client::new())?);
    let news = NewsRepository::new(pool.clone(), embedder.clone());
    let knowledge = KnowledgeRepository::new(pool.clone(), embedder.clone());
    let memory = MemoryService::new(Arc::new(PgMemoryStore::new(pool)), embedder);
    drain("news", || news.embed_missing_batch(BATCH)).await?;
    drain("knowledge", || knowledge.embed_missing_batch(BATCH)).await?;
    drain("memory", || memory.indexer().embed_missing_batch(BATCH)).await?;
    Ok(())
}
