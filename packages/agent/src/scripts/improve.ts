import "dotenv/config";
import { config as loadEnv } from "dotenv";
import { createAgentDb } from "../db/client";
import { createTraceStore } from "../db/trace-store";
import { createSkillStore } from "../skills";
import { createJudgeBackend, type JudgeProvider } from "../judging/judge-backend";
import { type NoiseAxis } from "../judging/noise";
import { JUDGE_PROMPT_VERSION } from "../judging/schema";
import { runImproveCycle } from "../judging/improve-cycle";
import { loadSigmaBaseline } from "../judging/sigma-baseline";
import { createLangfuseTraceSource, createLocalTraceSource, type TraceSource } from "../judging/trace-source";

loadEnv({ path: ".env.agent" });

// Closed-loop improver CLI (Phase 3): read the judged corpus for one (skill,
// axis), select recent failures (absolute+σ), induce a failure-mode taxonomy,
// author an append-only lesson for the dominant mode, GATE it (replay over the
// frozen cluster + holdout, stored before, Δ vs σ), and — with --apply — ship it
// to skills/<skill>.patch.md. Without --apply it's propose-only. The cron worker
// (improve:worker) drives the SAME runImproveCycle per (skill, axis) on a loop,
// plus the live-trend monitor + auto-revert that span runs.
//
//   pnpm improve --skill news-digest --axis coverage [--cluster 3] [--holdout 3]
//                [--samples 2] [--absMax 0.6] [--bar 0.75] [--holdoutMin 0.85]
//                [--recentDays 14] [--k 2] [--budget 8] [--maxAttempts 2]
//                [--provider codex] [--apply]

interface CliOpts {
  skill: string;
  axis: NoiseAxis;
  cluster: number;
  holdout: number;
  samples: number;
  absMax: number;
  bar: number;
  holdoutMin: number;
  recentDays: number;
  k: number;
  budget: number;
  maxAttempts: number;
  guardFaithfulness: boolean;
  provider: JudgeProvider;
  apply: boolean;
}

function parseArgs(argv: string[]): CliOpts {
  const o: Partial<CliOpts> = {
    cluster: 3,
    holdout: 3,
    samples: 2,
    absMax: 0.6,
    bar: 0.75,
    holdoutMin: 0.85,
    recentDays: 14,
    k: 2,
    budget: 8,
    maxAttempts: 1,
    guardFaithfulness: false,
    provider: "codex",
    apply: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    const val = (f: string): string => (arg.startsWith(`${f}=`) ? arg.slice(f.length + 1) : (argv[++i] ?? ""));
    if (arg === "--skill" || arg.startsWith("--skill=")) o.skill = val("--skill");
    else if (arg === "--axis" || arg.startsWith("--axis=")) o.axis = val("--axis") as NoiseAxis;
    else if (arg === "--cluster" || arg.startsWith("--cluster=")) o.cluster = Math.max(1, Number(val("--cluster")) || 3);
    else if (arg === "--holdout" || arg.startsWith("--holdout=")) o.holdout = Math.max(0, Number(val("--holdout")) || 3);
    else if (arg === "--samples" || arg.startsWith("--samples=")) o.samples = Math.max(1, Number(val("--samples")) || 2);
    else if (arg === "--absMax" || arg.startsWith("--absMax=")) o.absMax = Number(val("--absMax")) || 0.6;
    else if (arg === "--bar" || arg.startsWith("--bar=")) o.bar = Number(val("--bar")) || 0.75;
    else if (arg === "--holdoutMin" || arg.startsWith("--holdoutMin=")) o.holdoutMin = Number(val("--holdoutMin")) || 0.85;
    else if (arg === "--recentDays" || arg.startsWith("--recentDays=")) o.recentDays = Math.max(0, Number(val("--recentDays")) || 14);
    else if (arg === "--k" || arg.startsWith("--k=")) o.k = Math.max(0, Number(val("--k")) || 2);
    else if (arg === "--budget" || arg.startsWith("--budget=")) o.budget = Math.max(1, Number(val("--budget")) || 8);
    else if (arg === "--maxAttempts" || arg.startsWith("--maxAttempts=")) o.maxAttempts = Math.max(1, Number(val("--maxAttempts")) || 1);
    else if (arg === "--guard-faithfulness") o.guardFaithfulness = true;
    else if (arg === "--provider" || arg.startsWith("--provider=")) {
      const v = val("--provider");
      if (v !== "openai" && v !== "codex") throw new Error("--provider must be openai or codex");
      o.provider = v;
    } else if (arg === "--apply") o.apply = true;
    else throw new Error(`unknown flag: ${arg}`);
  }
  if (!o.skill || !o.axis) {
    console.error(
      "usage: pnpm improve --skill <skill> --axis <axis> [--cluster N] [--holdout M] " +
        "[--samples K] [--absMax 0.6] [--bar 0.75] [--holdoutMin 0.85] [--recentDays 14] " +
        "[--k 2] [--budget 8] [--maxAttempts 1] [--guard-faithfulness] [--provider codex|openai] [--apply]",
    );
    process.exit(1);
  }
  return o as CliOpts;
}

async function main(): Promise<void> {
  const opts = parseArgs(process.argv.slice(2));

  const db = createAgentDb();
  const store = createTraceStore(db);
  const skillStore = createSkillStore();
  const source: TraceSource = createLocalTraceSource(store, createLangfuseTraceSource());
  const backend = createJudgeBackend(opts.provider);

  const judgeModel = opts.provider === "openai" ? "openai" : (process.env.CODEX_JUDGE_MODEL ?? "codex");
  const { sigma } = loadSigmaBaseline(judgeModel);
  const axisSigma = sigma[opts.axis] ?? null;

  console.log(`\n=== IMPROVER · skill=${opts.skill} · axis=${opts.axis} ===`);
  console.log(
    `judge=${opts.provider} prompt=${JUDGE_PROMPT_VERSION} · σ=${axisSigma === null ? "—" : axisSigma.toFixed(3)} · ` +
      `candidate<${opts.absMax} & <${opts.bar}−${opts.k}σ · recent=${opts.recentDays}d · cluster≤${opts.cluster} · ` +
      `holdout≥${opts.holdoutMin} ×${opts.holdout} · samples=${opts.samples}/1 · budget=${opts.budget} · apply=${opts.apply}`,
  );

  const result = await runImproveCycle(
    { store, source, skillStore, backend, sigma, log: (m) => console.log(m) },
    { ...opts, now: Date.now() },
  );

  console.log(`\n── outcome: ${result.outcome} ──`);
  if (result.outcome === "shipped") {
    console.log(`Shipped to ${result.patchPath}. Monitor the live ${opts.axis} trend; revert = delete the lesson/file.`);
  } else if (result.outcome === "accepted") {
    console.log(`Accepted (propose-only). Re-run with --apply to ship to skills/${opts.skill}.patch.md.`);
  }
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
