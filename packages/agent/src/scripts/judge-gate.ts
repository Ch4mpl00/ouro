import "dotenv/config";
import { readFileSync } from "node:fs";
import { config as loadEnv } from "dotenv";
import { createAgentDb } from "../db/client";
import { createTraceStore } from "../db/trace-store";
import { assembleNodeMaterials } from "../judging/materials";
import { createJudgeBackend, type JudgeProvider } from "../judging/judge-backend";
import { runNodeGate } from "../judging/gate";
import { type NoiseAxis } from "../judging/noise";
import { JUDGE_MODEL, JUDGE_PROMPT_VERSION } from "../judging/schema";
import {
  createLangfuseTraceSource,
  createLocalTraceSource,
  type TraceSource,
} from "../judging/trace-source";
import type { Observation } from "../trace-model";
import { buildGateTarget, runModel } from "./gate-runtime";

loadEnv({ path: ".env.agent" });

// Improver gate CLI (Phase 2, slice 1): A/B a CANDIDATE patch on a skill's nodes
// over a frozen recorded trace, re-judge, and report whether the target axis
// moved beyond σ_judge (the noise floor from `pnpm judge:noise`).
//
//   pnpm judge:gate <traceId> --skill <skill> --patch <file.md>
//                   [--axis <axis>] [--samples N] [--provider codex|openai] [--k 2]
//
// The generator is re-run under the RECORDED model (what prod uses); the judge
// defaults to codex (prod judge, cheap). Δ is graded against the committed
// noise baseline for (judge model | prompt version).

const BASELINE_PATH = "packages/agent/src/judging/noise-baseline.json";

interface CliOpts {
  traceId: string;
  skill: string;
  patchFile: string;
  axis: NoiseAxis | null;
  node: string | null;
  samples: number;
  provider: JudgeProvider;
  k: number;
}

function parseArgs(argv: string[]): CliOpts {
  const opts: Partial<CliOpts> = { provider: "codex", samples: 3, k: 2, axis: null, node: null };
  const rest: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    const val = (flag: string): string =>
      arg.startsWith(`${flag}=`) ? arg.slice(flag.length + 1) : (argv[++i] ?? "");
    if (arg === "--skill" || arg.startsWith("--skill=")) opts.skill = val("--skill");
    else if (arg === "--patch" || arg.startsWith("--patch=")) opts.patchFile = val("--patch");
    else if (arg === "--node" || arg.startsWith("--node=")) opts.node = val("--node");
    else if (arg === "--axis" || arg.startsWith("--axis=")) opts.axis = val("--axis") as NoiseAxis;
    else if (arg === "--samples" || arg.startsWith("--samples=")) opts.samples = Math.max(1, Number(val("--samples")) || 3);
    else if (arg === "--k" || arg.startsWith("--k=")) opts.k = Math.max(0, Number(val("--k")) || 2);
    else if (arg === "--provider" || arg.startsWith("--provider=")) {
      const v = val("--provider");
      if (v !== "openai" && v !== "codex") throw new Error("--provider must be openai or codex");
      opts.provider = v;
    } else if (arg.startsWith("--")) throw new Error(`unknown flag: ${arg}`);
    else rest.push(arg);
  }
  opts.traceId = rest[0];
  if (!opts.traceId || !opts.skill || !opts.patchFile) {
    console.error(
      "usage: pnpm judge:gate <traceId> --skill <skill> --patch <file.md> " +
        "[--node <labelSubstr>] [--axis <axis>] [--samples N] [--provider codex|openai] [--k 2]",
    );
    process.exit(1);
  }
  return opts as CliOpts;
}

// ─── σ baseline lookup ───────────────────────────────────────────────

interface BaselineAxis {
  axis: NoiseAxis;
  pooledSigma: number;
}
interface BaselineEntry {
  model: string;
  axes: BaselineAxis[];
}

function loadSigma(judgeModel: string): { sigma: Partial<Record<NoiseAxis, number>>; found: boolean } {
  let entries: Record<string, BaselineEntry> = {};
  try {
    entries = JSON.parse(readFileSync(BASELINE_PATH, "utf-8")) as Record<string, BaselineEntry>;
  } catch {
    return { sigma: {}, found: false };
  }
  const entry = entries[`${judgeModel}|${JUDGE_PROMPT_VERSION}`];
  if (!entry) return { sigma: {}, found: false };
  const sigma: Partial<Record<NoiseAxis, number>> = {};
  for (const a of entry.axes) sigma[a.axis] = a.pooledSigma;
  return { sigma, found: true };
}

const VERDICT_GLYPH: Record<string, string> = {
  improve: "✅ improve",
  regress: "🔻 regress",
  noise: "≈ noise",
  "no-baseline": "? no-baseline",
  "n/a": "— n/a",
};

async function main(): Promise<void> {
  const opts = parseArgs(process.argv.slice(2));
  const patch = readFileSync(opts.patchFile, "utf-8");

  const db = createAgentDb();
  const source: TraceSource = createLocalTraceSource(createTraceStore(db), createLangfuseTraceSource());

  const { trace, observations } = await source.getTrace(opts.traceId);
  const byId = new Map<string, Observation>(observations.map((o) => [o.id, o]));
  const { nodes } = await assembleNodeMaterials(source, opts.traceId);

  const targets = nodes
    .filter((n) => n.skill === opts.skill)
    .filter((n) => (opts.node ? n.label.includes(opts.node) : true));
  if (targets.length === 0) {
    console.error(
      `[judge:gate] no nodes for skill "${opts.skill}" in ${opts.traceId}. ` +
        `Skills present: ${[...new Set(nodes.map((n) => n.skill))].join(", ") || "(none)"}`,
    );
    process.exit(1);
  }

  const judgeModel = opts.provider === "openai" ? JUDGE_MODEL : (process.env.CODEX_JUDGE_MODEL ?? "codex");
  const { sigma, found } = loadSigma(judgeModel);

  console.log(`\n=== IMPROVER GATE · trace ${opts.traceId} · ${trace.name} ===`);
  console.log(
    `skill=${opts.skill} · patch=${opts.patchFile} (${patch.trim().length} chars) · ` +
      `judge=${opts.provider}(${judgeModel}) prompt=${JUDGE_PROMPT_VERSION} · samples=${opts.samples} · k=${opts.k}`,
  );
  if (!found) {
    console.log(
      `⚠ no σ baseline for ${judgeModel}|${JUDGE_PROMPT_VERSION} — run \`pnpm judge:noise --provider ${opts.provider}\` ` +
        `first; verdicts will read "no-baseline".`,
    );
  }
  console.log(`${targets.length} target node(s)\n`);

  const backend = createJudgeBackend(opts.provider);

  for (const node of targets) {
    const target = buildGateTarget(node, byId.get(node.observationId));
    if (!target) {
      console.log(`── ${node.label} · SKIP (no recorded model and AGENT_MODEL unset) ──\n`);
      continue;
    }
    console.log(`── node ${node.label} · ${node.kind} · model ${target.model} ──`);
    const result = await runNodeGate({ backend, runModel }, target, patch, opts.samples, sigma, opts.k);

    const shown = opts.axis ? result.grades.filter((g) => g.axis === opts.axis) : result.grades;
    for (const g of shown) {
      const fmt = (x: number | null) => (x === null ? " —  " : x.toFixed(3));
      const dz =
        g.delta !== null && g.sigma ? ` (${(g.delta / g.sigma).toFixed(1)}σ)` : "";
      console.log(
        `  ${g.axis.padEnd(18)} before ${fmt(g.beforeMean)} → after ${fmt(g.afterMean)}  ` +
          `Δ ${g.delta === null ? " — " : (g.delta >= 0 ? "+" : "") + g.delta.toFixed(3)}${dz}  ` +
          `[${VERDICT_GLYPH[g.verdict] ?? g.verdict}]`,
      );
    }
    console.log();
  }

  console.log(
    "Gate rule: accept the patch only if the TARGET axis is `improve` AND no other " +
      "axis flips to `regress` (holdout regression check comes in Phase 3).",
  );
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
