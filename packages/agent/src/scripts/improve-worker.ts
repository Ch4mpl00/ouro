import "dotenv/config";
import { config as loadEnv } from "dotenv";
import { createAgentDb } from "../db/client";
import { createTraceStore } from "../db/trace-store";
import { createImproverStore } from "../db/improver-store";
import { createSkillStore } from "../skills";
import { createJudgeBackend } from "../judging/judge-backend";
import { loadSigmaBaseline } from "../judging/sigma-baseline";
import { improveWorkerOptsFromEnv, runImproveWorker } from "../judging/improve-worker";
import { createLangfuseTraceSource, createLocalTraceSource } from "../judging/trace-source";

loadEnv({ path: ".env.agent" });

// Closed-loop improver cron (Phase 3, п3): the long-running poll loop that walks
// every (skill, axis) in the judged corpus, monitors the prod trend of the last
// ship (auto-reverting if it didn't hold), and otherwise runs one improve cycle.
// Mirrors judge-worker as a compose service. Ships ONLY when IMPROVE_APPLY is set
// — otherwise shadow mode (proposes + gates, never writes a .patch.md).

async function main(): Promise<void> {
  const opts = improveWorkerOptsFromEnv();
  const db = createAgentDb();
  try {
    const store = createTraceStore(db);
    const improverStore = createImproverStore(db);
    const skillStore = createSkillStore();
    const langfuseEnabled = Boolean(process.env.LANGFUSE_PUBLIC_KEY && process.env.LANGFUSE_SECRET_KEY);
    const source = createLocalTraceSource(store, langfuseEnabled ? createLangfuseTraceSource() : undefined);
    const backend = createJudgeBackend(opts.provider);

    const judgeModel = opts.provider === "openai" ? "openai" : (process.env.CODEX_JUDGE_MODEL ?? "codex");
    const { sigma, found } = loadSigmaBaseline(judgeModel);
    if (!found) {
      console.warn(
        `[improve-worker] no σ baseline for ${judgeModel} — gate verdicts read "no-baseline" and the ` +
          `monitor can't bound noise. Run \`pnpm judge:noise --provider ${opts.provider}\` first.`,
      );
    }

    await runImproveWorker(
      { store, improverStore, skillStore, source, backend, sigma, log: (m) => console.log(m) },
      opts,
    );
  } finally {
    db.$client.close();
  }
}

main().catch((err: unknown) => {
  console.error("[improve-worker] fatal:", err);
  process.exit(1);
});
