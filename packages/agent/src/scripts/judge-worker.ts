import "dotenv/config";
import "../openai-native-fetch";
import { config as loadEnv } from "dotenv";
import {
  createAgentDb,
  createTraceStore,
} from "../db";
import {
  createLangfuseTraceSource,
  createLocalTraceSource,
  createScoreWriter,
  judgeWorkerOptsFromEnv,
  runJudgeWorker,
} from "../judging";

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
