import "dotenv/config";
import { createPgClient } from "../db/pg/client";
import { createNewsModule } from "../services/news";
import { createKnowledgeModule } from "../services/knowledge";
import { createMemoryModule } from "../services/memory";
import { drainMissing } from "../services/embeddings/backfill";

// Re-attempts embeddings for rows where the inline embed previously
// failed (embedding IS NULL), across every embedded store. Safe to
// re-run.
//
//   docker compose exec -w /app mcp pnpm embed:backfill

async function main(): Promise<void> {
  const pg = createPgClient();
  await pg.ensureReady();
  const { repository: news } = createNewsModule({ db: pg.db });
  const { repository: knowledge } = createKnowledgeModule({ db: pg.db });
  const { indexer: memory } = createMemoryModule({ db: pg.db });

  try {
    await drainMissing("news", news.embedMissingBatch);
    await drainMissing("knowledge", knowledge.embedMissingBatch);
    await drainMissing("memory", memory.embedMissingBatch);
  } finally {
    await pg.close();
  }
}

main().catch((err) => {
  console.error("[embed-backfill] crashed:", err);
  process.exitCode = 1;
});
