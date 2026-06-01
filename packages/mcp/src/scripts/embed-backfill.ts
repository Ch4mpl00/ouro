import "dotenv/config";
import { createPgClient } from "../db/pg/client";
import { createNewsModule } from "../services/news";

// Re-attempts embeddings for rows where the inline embed previously
// failed (embedding IS NULL). Safe to re-run.
//
//   docker compose exec -w /app mcp pnpm embed:backfill

const BATCH = 100;

async function main(): Promise<void> {
  const pg = createPgClient();
  await pg.ensureReady();
  const { repository: news } = createNewsModule({ db: pg.db });

  let totalEmbedded = 0;
  let totalFailed = 0;

  try {
    for (;;) {
      const result = await news.embedMissingBatch(BATCH);
      if (result.embedded === 0 && result.failed === 0) break;
      totalEmbedded += result.embedded;
      totalFailed += result.failed;
      console.log(
        `[embed-backfill] batch: embedded=${result.embedded}, failed=${result.failed} (running totals: ${totalEmbedded}/${totalFailed})`,
      );
      if (result.failed > 0) {
        // Whole-batch failure → likely auth/API problem; retrying won't help.
        console.error("[embed-backfill] giving up after batch failure");
        break;
      }
    }
    console.log(`[embed-backfill] done: embedded=${totalEmbedded}, failed=${totalFailed}`);
  } finally {
    await pg.close();
  }
}

main().catch((err) => {
  console.error("[embed-backfill] crashed:", err);
  process.exitCode = 1;
});
