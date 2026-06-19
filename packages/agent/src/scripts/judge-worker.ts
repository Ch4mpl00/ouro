import "dotenv/config";
import "../openai-native-fetch";
import { config as loadEnv } from "dotenv";
import { createAgentDb } from "../db/client";
import { createTraceStore } from "../db/trace-store";
import { createScoreWriter } from "../judging/langfuse-scores";
import { createLangfuseTraceSource, createLocalTraceSource } from "../judging/trace-source";
import { judgeWorkerOptsFromEnv, runJudgeWorker } from "../judging/worker";

loadEnv({ path: ".env.agent" });

async function main(): Promise<void> {
  const db = createAgentDb();
  try {
    const store = createTraceStore(db);
    // Langfuse is optional: used as a getTrace fallback for ids not mirrored
    // locally, and as the scores sink. Listing always comes from local.
    const langfuseEnabled = Boolean(
      process.env.LANGFUSE_PUBLIC_KEY && process.env.LANGFUSE_SECRET_KEY,
    );
    const source = createLocalTraceSource(
      store,
      langfuseEnabled ? createLangfuseTraceSource() : undefined,
    );
    const writeScores = createScoreWriter({ store, langfuseEnabled });

    await runJudgeWorker({ source, writeScores }, judgeWorkerOptsFromEnv());
  } finally {
    db.$client.close();
  }
}

main().catch((err: unknown) => {
  console.error("[judge-worker] fatal:", err);
  process.exit(1);
});
