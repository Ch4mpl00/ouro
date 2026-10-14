// Re-attempts embeddings left NULL by a failed inline embed, in every
// embedded store: one pass in id order, so a row the embedder rejects for
// good holds up nothing behind it. Safe to re-run.
//
//   docker compose exec mcp embed-backfill

use std::sync::Arc;

use mcp_tools::embeddings::{OpenAiEmbedder, SharedEmbedder, drain_missing};
use mcp_tools::knowledge::KnowledgeRepository;
use mcp_tools::memory::{MemoryService, PgMemoryStore};
use mcp_tools::news::NewsRepository;
use mcp_tools::{cli, pg};

const BATCH: i64 = 100;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let pool = pg::connect(&pg::database_url()?)?;
    pg::migrate(&pool).await?;
    let embedder: SharedEmbedder = Arc::new(OpenAiEmbedder::from_env(reqwest::Client::new())?);
    let news = NewsRepository::new(pool.clone(), embedder.clone());
    let knowledge = KnowledgeRepository::new(pool.clone(), embedder.clone());
    let memory = MemoryService::new(Arc::new(PgMemoryStore::new(pool)), embedder);
    let log = |line: &str| println!("{line}");
    drain_missing("news", BATCH, &log, |batch, after| news.embed_missing_batch(batch, after)).await?;
    drain_missing("knowledge", BATCH, &log, |batch, after| knowledge.embed_missing_batch(batch, after)).await?;
    drain_missing("memory", BATCH, &log, |batch, after| memory.indexer().embed_missing_batch(batch, after)).await?;
    Ok(())
}
