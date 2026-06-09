import "dotenv/config";
import { createPgClient } from "../db/pg/client";
import { createNewsModule } from "../services/news";
import { createKnowledgeModule } from "../services/knowledge";

// Re-attempts embeddings for rows where the inline embed previously
// failed (embedding IS NULL), across every embedded store. Safe to
// re-run.
//
//   docker compose exec -w /app mcp pnpm embed:backfill

const BATCH = 100;

interface EmbedResult {
  embedded: number;
  failed: number;
}

// Drains one store's NULL-embedding backlog one batch at a time, until
// empty or a whole-batch failure (a batch failing wholesale points at an
// auth/API problem that retrying won't fix).
async function drain(
  label: string,
  embedMissingBatch: (batchSize?: number) => Promise<EmbedResult>,
): Promise<void> {
  let totalEmbedded = 0;
  let totalFailed = 0;
  for (;;) {
    const result = await embedMissingBatch(BATCH);
    if (result.embedded === 0 && result.failed === 0) break;
    totalEmbedded += result.embedded;
    totalFailed += result.failed;
    console.log(
      `[embed-backfill:${label}] batch: embedded=${result.embedded}, failed=${result.failed} (running totals: ${totalEmbedded}/${totalFailed})`,
    );
    if (result.failed > 0) {
      console.error(`[embed-backfill:${label}] giving up after batch failure`);
      break;
    }
  }
  console.log(
    `[embed-backfill:${label}] done: embedded=${totalEmbedded}, failed=${totalFailed}`,
  );
}

async function main(): Promise<void> {
  const pg = createPgClient();
  await pg.ensureReady();
  const { repository: news } = createNewsModule({ db: pg.db });
  const { repository: knowledge } = createKnowledgeModule({ db: pg.db });

  try {
    await drain("news", news.embedMissingBatch);
    await drain("knowledge", knowledge.embedMissingBatch);
  } finally {
    await pg.close();
  }
}

main().catch((err) => {
  console.error("[embed-backfill] crashed:", err);
  process.exitCode = 1;
});
