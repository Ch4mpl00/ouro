import "dotenv/config";
import { readFileSync } from "node:fs";
import { config as loadEnv } from "dotenv";
import { z } from "zod";
import { createAgentDb } from "../db/client";
import { createTraceStore, type JudgementRecord } from "../db/trace-store";
import { createSkillStore } from "../skills";
import { assembleNodeMaterials, type NodeMaterial } from "../judging/materials";
import { createJudgeBackend, type JudgeProvider } from "../judging/judge-backend";
import { runNodeGate, type NodeGateResult } from "../judging/gate";
import { type NoiseAxis } from "../judging/noise";
import { JUDGE_PROMPT_VERSION } from "../judging/schema";
import {
  authorPatch,
  decideShip,
  dominantMode,
  induceTaxonomy,
  selectCandidates,
  type PatchExample,
} from "../judging/improver";
import {
  createLangfuseTraceSource,
  createLocalTraceSource,
  type TraceSource,
} from "../judging/trace-source";
import type { Observation } from "../trace-model";
import { buildGateTarget, runModel } from "./gate-runtime";

loadEnv({ path: ".env.agent" });

// Closed-loop improver (Phase 3, п2): read the judged corpus for a skill+axis,
// cluster the low scorers, author an append-only patch, GATE it (replay over the
// frozen cluster + a high-score holdout, re-judge, Δ vs σ), and — with --apply —
// ship it to skills/<skill>.patch.md. Without --apply it's propose-only (prints
// the candidate + the gate verdict). The cron (п3) just calls this on a schedule.
//
//   pnpm improve --skill news-digest --axis coverage [--cluster 3] [--holdout 3]
//                [--samples 2] [--absMax 0.6] [--bar 0.75] [--holdoutMin 0.85]
//                [--recentDays 14] [--k 2] [--provider codex] [--apply]

interface CliOpts {
  skill: string;
  axis: NoiseAxis;
  cluster: number; // cap on the cluster (dominant-mode nodes the gate replays)
  holdout: number;
  samples: number; // cluster samples (S); holdout is always S=1 (regression guard)
  absMax: number; // candidate iff score < absMax …
  bar: number; // … AND score < bar − k·σ (bar = "ok" anchor by default)
  holdoutMin: number; // all-time gold-standard threshold
  recentDays: number; // cluster drawn from this trailing window
  k: number;
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
        "[--k 2] [--provider codex|openai] [--apply]",
    );
    process.exit(1);
  }
  return o as CliOpts;
}

// Pull the judge's rationale for one axis out of the stored detail JSON (parsed
// with zod, no casts — detail is the scorecard + faithfulness payload).
const DetailSchema = z.object({
  scorecard: z
    .object({
      axes: z.array(
        z.object({ axis: z.string(), rationale: z.string().optional(), evidence: z.string().optional() }),
      ),
    })
    .optional(),
  faithfulness: z.object({ note: z.string().optional() }).nullable().optional(),
});

function judgeRationale(detail: unknown, axis: NoiseAxis): string {
  const p = DetailSchema.safeParse(detail);
  if (!p.success) return "";
  if (axis === "faithfulness") return p.data.faithfulness?.note ?? "";
  const a = p.data.scorecard?.axes.find((x) => x.axis === axis);
  if (!a) return "";
  return `${a.rationale ?? ""}${a.evidence ? ` (evidence: ${a.evidence})` : ""}`.trim();
}

async function main(): Promise<void> {
  const opts = parseArgs(process.argv.slice(2));

  const db = createAgentDb();
  const store = createTraceStore(db);
  const skillStore = createSkillStore();
  const source: TraceSource = createLocalTraceSource(store, createLangfuseTraceSource());

  const judgeModel = opts.provider === "openai" ? "openai" : (process.env.CODEX_JUDGE_MODEL ?? "codex");
  const sigma = loadSigma(judgeModel);
  const axisSigma = sigma[opts.axis] ?? null;
  console.log(`\n=== IMPROVER · skill=${opts.skill} · axis=${opts.axis} ===`);
  console.log(
    `judge=${opts.provider} prompt=${JUDGE_PROMPT_VERSION} · σ=${axisSigma === null ? "—" : axisSigma.toFixed(3)} · ` +
      `candidate<${opts.absMax} & <${opts.bar}−${opts.k}σ · recent=${opts.recentDays}d · cluster≤${opts.cluster} · ` +
      `holdout≥${opts.holdoutMin} ×${opts.holdout} · samples=${opts.samples}/1 · apply=${opts.apply}`,
  );

  const records = store.listJudgements({
    skill: opts.skill,
    provider: opts.provider,
    promptVersion: JUDGE_PROMPT_VERSION,
  });
  if (records.length === 0) {
    console.log(
      `No judgements for (${opts.skill}, ${opts.provider}, ${JUDGE_PROMPT_VERSION}) in the local store. ` +
        `Run the judge worker first (this reads the local corpus).`,
    );
    return;
  }

  const recentSince =
    opts.recentDays > 0 ? new Date(Date.now() - opts.recentDays * 86_400_000).toISOString() : null;
  const { candidates, holdout } = selectCandidates(records, opts.axis, {
    holdoutSize: opts.holdout,
    absMax: opts.absMax,
    bar: opts.bar,
    k: opts.k,
    sigma: axisSigma,
    holdoutMin: opts.holdoutMin,
    recentSince,
  });
  console.log(
    `corpus=${records.length} · recent candidates=${candidates.length} · holdout=${holdout.length}`,
  );
  if (candidates.length === 0) {
    console.log(
      `No confident failures on ${opts.axis} in the recent window — the floor has reached the ceiling here, nothing to improve.`,
    );
    return;
  }

  // Taxonomy (open-coding): group the candidates by failure MODE and patch the
  // Pareto-dominant one — the lowest-N may fail for unrelated reasons that one
  // patch can't fix. A fresh taxonomy each run → self-adapts to what fails NOW.
  const nid = (r: JudgementRecord): string => `${r.traceId}:${r.observationId}`;
  const rationaleById = new Map(
    candidates.map((r) => [nid(r), judgeRationale(r.detail, opts.axis)]),
  );
  const backend = createJudgeBackend(opts.provider);
  console.log(`\nInducing failure-mode taxonomy over ${candidates.length} candidate(s)…`);
  const taxonomy = await induceTaxonomy(backend, {
    skill: opts.skill,
    axis: opts.axis,
    items: candidates.map((r) => ({ id: nid(r), rationale: rationaleById.get(nid(r)) ?? "" })),
  });
  for (const m of taxonomy.modes) {
    console.log(`  · ${m.name} (${m.nodeIds.filter((id) => rationaleById.has(id)).length}): ${m.description}`);
  }
  const mode = dominantMode(taxonomy, new Set(rationaleById.keys()));
  if (!mode) {
    console.log("\nTaxonomy produced no usable mode (no candidate mapped) — aborting.");
    return;
  }
  const modeIds = new Set(mode.nodeIds);
  const cluster = candidates.filter((r) => modeIds.has(nid(r))).slice(0, opts.cluster);
  console.log(
    `\nDominant mode: "${mode.name}" — ${mode.description}\n` +
      `cluster=${cluster.length} node(s) (capped at ${opts.cluster}); holdout=${holdout.length}`,
  );

  // Resolve each judged record to its node material (+ recorded observation),
  // caching per trace so a multi-node trace is fetched once.
  const traceCache = new Map<string, { nodes: NodeMaterial[]; byId: Map<string, Observation> }>();
  async function resolve(rec: JudgementRecord): Promise<{ node: NodeMaterial; obs: Observation | undefined } | null> {
    let entry = traceCache.get(rec.traceId);
    if (!entry) {
      const { observations } = await source.getTrace(rec.traceId);
      const { nodes } = await assembleNodeMaterials(source, rec.traceId);
      entry = { nodes, byId: new Map(observations.map((o) => [o.id, o])) };
      traceCache.set(rec.traceId, entry);
    }
    const node = entry.nodes.find((n) => n.observationId === rec.observationId);
    return node ? { node, obs: entry.byId.get(rec.observationId) } : null;
  }

  // Build the author's failing examples from the cluster.
  const examples: PatchExample[] = [];
  let contract: string | null = null;
  for (const rec of cluster) {
    const r = await resolve(rec);
    if (!r) continue;
    contract = r.node.contract;
    examples.push({
      inputExcerpt: r.node.inputText,
      output: r.node.outputText,
      judgeRationale: judgeRationale(rec.detail, opts.axis) || "(no rationale recorded)",
    });
  }
  if (examples.length === 0) {
    console.log("Could not resolve any cluster node to its trace material — aborting.");
    return;
  }

  // Show the author the current patch so it does NOT repeat an existing lesson
  // (dedup-on-input — pairs with retire-on-output, п3, to bound prompt growth).
  const existingPatch = (await skillStore.readPatch(opts.skill)) ?? "";
  console.log(`\nAuthoring patch from ${examples.length} failing example(s) of "${mode.name}"…`);
  const authored = await authorPatch(backend, {
    skill: opts.skill,
    axis: opts.axis,
    contract,
    examples,
    failureMode: mode.description,
    existingPatch,
  });
  console.log(`\n── candidate patch ──\n${authored.lesson || "(empty)"}`);
  console.log(`\nauthor rationale: ${authored.rationale}`);
  if (authored.lesson.trim().length === 0) {
    console.log("\nAuthor found no generalizable fix (or it's already patched) — nothing to gate.");
    return;
  }

  // Gate: replay the candidate over cluster (must improve target axis) and
  // holdout (must not regress). "before" = each node's STORED score (free); only
  // regeneration costs codex. σ baseline drives the per-axis verdicts. Cluster is
  // sampled S× (measurement), holdout S=1 (regression guard, not a measurement).
  async function gateNodes(recs: JudgementRecord[], samples: number): Promise<NodeGateResult[]> {
    const out: NodeGateResult[] = [];
    for (const rec of recs) {
      const r = await resolve(rec);
      if (!r) continue;
      const target = buildGateTarget(r.node, r.obs);
      if (!target) continue;
      out.push(
        await runNodeGate({ backend, runModel }, target, authored.lesson, samples, sigma, opts.k, rec.scores),
      );
    }
    return out;
  }

  console.log(`\nGating candidate over cluster (${cluster.length}, S=${opts.samples}) + holdout (${holdout.length}, S=1)…`);
  const clusterResults = await gateNodes(cluster, opts.samples);
  const holdoutResults = await gateNodes(holdout, 1);

  printGate("cluster", clusterResults, opts.axis);
  printGate("holdout", holdoutResults, opts.axis);

  const decision = decideShip(opts.axis, clusterResults, holdoutResults);
  console.log(`\n── decision ──`);
  for (const r of decision.reasons) console.log(`  • ${r}`);

  if (!decision.accept) {
    console.log(`\nREJECTED — not shipping.`);
    return;
  }
  if (!opts.apply) {
    console.log(`\nACCEPTED (propose-only). Re-run with --apply to ship to skills/${opts.skill}.patch.md.`);
    return;
  }

  const merged =
    existingPatch.trim().length > 0
      ? `${existingPatch.trimEnd()}\n\n${authored.lesson.trim()}\n`
      : `${authored.lesson.trim()}\n`;
  const saved = await skillStore.savePatch(opts.skill, merged);
  console.log(`\nSHIPPED → ${saved.path} (${saved.sizeBytes} bytes). Monitor the live ${opts.axis} trend; revert = delete the file.`);
}

function loadSigma(judgeModel: string): Partial<Record<NoiseAxis, number>> {
  try {
    const entries = JSON.parse(readFileSync("packages/agent/src/judging/noise-baseline.json", "utf-8")) as Record<
      string,
      { axes: Array<{ axis: NoiseAxis; pooledSigma: number }> }
    >;
    const entry = entries[`${judgeModel}|${JUDGE_PROMPT_VERSION}`];
    if (!entry) return {};
    const sigma: Partial<Record<NoiseAxis, number>> = {};
    for (const a of entry.axes) sigma[a.axis] = a.pooledSigma;
    return sigma;
  } catch {
    return {};
  }
}

function printGate(label: string, results: NodeGateResult[], axis: NoiseAxis): void {
  console.log(`\n${label} (${results.length} node(s)):`);
  for (const n of results) {
    const g = n.grades.find((x) => x.axis === axis);
    if (!g) continue;
    const fmt = (x: number | null) => (x === null ? "—" : x.toFixed(3));
    console.log(
      `  ${n.node.label.padEnd(22)} ${axis} ${fmt(g.beforeMean)}→${fmt(g.afterMean)} ` +
        `Δ${g.delta === null ? "—" : (g.delta >= 0 ? "+" : "") + g.delta.toFixed(3)} [${g.verdict}]`,
    );
  }
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
