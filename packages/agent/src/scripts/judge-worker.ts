import "dotenv/config";
import { config as loadEnv } from "dotenv";
import { createAgentDb } from "../db/client";
import { createMemoryStore } from "../db/memory";
import { judgeWorkerOptsFromEnv, runJudgeWorker } from "../judging/worker";

loadEnv({ path: ".env.agent" });

async function main(): Promise<void> {
  const db = createAgentDb();
  try {
    await runJudgeWorker(
      { memory: createMemoryStore(db) },
      judgeWorkerOptsFromEnv(),
    );
  } finally {
    db.close();
  }
}

main().catch((err: unknown) => {
  console.error("[judge-worker] fatal:", err);
  process.exit(1);
});
