// Copies knowledge_base_notes into memory facts. Idempotent: provenance on
// each fact makes a re-run skip what is already there. The source table is
// left untouched.

use std::sync::Arc;

use mcp_tools::embeddings::{OpenAiEmbedder, SharedEmbedder};
use mcp_tools::knowledge::KnowledgeRepository;
use mcp_tools::memory::{LegacyNote, MemoryService, PgMemoryStore, import_legacy_notes};
use mcp_tools::{cli, pg};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let pool = pg::connect(&pg::database_url()?)?;
    pg::migrate(&pool).await?;
    let embedder: SharedEmbedder = Arc::new(OpenAiEmbedder::from_env(reqwest::Client::new())?);
    let notes: Vec<LegacyNote> = KnowledgeRepository::new(pool.clone(), embedder.clone())
        .all_notes()
        .await?
        .into_iter()
        .map(|(id, body, tags, _)| LegacyNote { id, body, tags })
        .collect();
    println!("[memory-import] {} note(s) in knowledge_base_notes", notes.len());
    let memory = MemoryService::new(Arc::new(PgMemoryStore::new(pool)), embedder);
    let (imported, skipped) = import_legacy_notes(&notes, &memory).await?;
    println!("[memory-import] done: imported={imported}, skipped={skipped}");
    if imported > 0 {
        println!("[memory-import] run `embed-backfill` if the embedder was down during the import");
    }
    Ok(())
}
